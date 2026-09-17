//! Corruption-specific DB circuit breaker (br-bvq1x.11.3 / K3).
//!
//! Once a HARD, edit-blocking corruption-class error (A1) is observed on the
//! database path, this breaker trips and STAYS tripped: subsequent writes are
//! refused immediately — without touching the database again — so agents stop
//! hammering a corrupt store (re-emitting scary errors and risking worse
//! damage). The refusal carries the corruption keywords of the triggering
//! error, so it re-classifies as corruption and renders the structured A2
//! envelope plus `am doctor repair` / `reconstruct` guidance.
//!
//! This is deliberately distinct from the auto-recovering, time-reset
//! per-subsystem breakers in [`crate::retry`] (which are for transient
//! contention, classed `RESOURCE_BUSY`). Corruption must NOT auto-recover on a
//! timer. Automatic recovery requires the live-mailbox owner's full check
//! and an unchanged corruption epoch. Quick checks and diagnostic copies do
//! not release writes. Explicit operator reset or process restart is separate.
//!
//! ## Scope: writes only, server only
//!
//! The refusal applies solely to the async server write retry path. Read
//! retries observe corruption but remain available when the breaker is open.
//! It is also process-local: the CLI/doctor sync write path runs in a
//! separate process whose breaker is never tripped, so recovery via `am doctor`
//! is never blocked.

#![forbid(unsafe_code)]

use crate::error::{DbError, DbErrorClass};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn now_micros_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_micros()).ok())
        .unwrap_or(0)
}

/// Evidence for one open epoch. All fields are published and cleared together.
#[derive(Debug, Clone)]
struct TripDetail {
    class: DbErrorClass,
    message: String,
    tripped_at_us: u64,
}

#[derive(Debug, Default)]
struct BreakerState {
    /// Lifetime count, deliberately retained across verified resets.
    trip_count: u64,
    detail: Option<TripDetail>,
}

/// A sticky, corruption-only circuit breaker.
#[derive(Debug, Default)]
pub struct CorruptionCircuitBreaker {
    // Keep the healthy admission check lock-free. Transitions and readers of
    // diagnostic evidence use `state`; the atomic is published while holding
    // that same lock, only after the corresponding state is complete.
    tripped: AtomicBool,
    state: Mutex<BreakerState>,
}

impl CorruptionCircuitBreaker {
    fn lock_state(&self) -> MutexGuard<'_, BreakerState> {
        // A previous panic must not discard the original corruption evidence
        // or turn an open breaker into an apparently healthy one. No caller
        // conversion or tracing subscriber runs while this lock is held.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Trip the breaker for a hard corruption-class error. Idempotent: only the
    /// first trip records the triggering detail; later corruption errors bump
    /// the count but keep the original cause.
    pub fn trip(&self, class: DbErrorClass, message: impl Into<String>) {
        // Conversion can execute caller code. Complete it before claiming an
        // epoch, so a delayed conversion cannot overwrite a newer cause after
        // a concurrent reset/retrip, or deadlock a reentrant observer.
        let message = message.into();
        let opened = {
            let mut state = self.lock_state();
            state.trip_count = state.trip_count.saturating_add(1);
            if state.detail.is_none() {
                state.detail = Some(TripDetail {
                    class,
                    message: message.clone(),
                    tripped_at_us: now_micros_u64(),
                });
                self.tripped.store(true, Ordering::Release);
                // Publish the flag before unlocking so reset cannot interleave.
                drop(state);
                true
            } else {
                false
            }
        };
        if opened {
            tracing::error!(
                class = class.as_str(),
                error = %message,
                "database corruption circuit breaker opened; writes refused, reads remain available"
            );
        }
    }

    /// Trip from a [`DbError`] iff it is a hard, edit-blocking corruption.
    /// Returns true if the error qualified (regardless of whether it newly
    /// tripped the breaker).
    pub fn observe_error(&self, error: &DbError) -> bool {
        let classification = error.classification();
        if error.is_corruption() && classification.blocks_edits {
            self.trip(classification.class, error.to_string());
            true
        } else {
            false
        }
    }

    /// Whether the breaker is currently open.
    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// Total number of corruption errors observed over this breaker's lifetime.
    /// Verified resets retain this counter; it saturates instead of wrapping.
    #[must_use]
    pub fn trip_count(&self) -> u64 {
        self.lock_state().trip_count
    }

    /// When open, the [`DbError`] a write should be refused with: a
    /// corruption-class error whose message preserves the triggering
    /// corruption keywords (so it re-classifies as corruption and renders the
    /// A2 envelope) plus explicit degraded-mode guidance. `None` when closed.
    #[must_use]
    pub fn refusal_error(&self) -> Option<DbError> {
        if !self.is_tripped() {
            return None;
        }
        let original = {
            let state = self.lock_state();
            // A reset may have completed after the optimistic atomic read.
            // Never combine an old open flag with another epoch's evidence.
            state.detail.as_ref()?.message.clone()
        };
        Some(DbError::Sqlite(format!(
            "corruption circuit breaker open: refusing this write to avoid worsening damage to a \
             corrupt database. Reads remain available; run `am doctor repair` or `am doctor \
             reconstruct` to recover, then a clean integrity check (or a restart) clears the \
             breaker. Triggering error: {original}"
        )))
    }

    /// Capture the corruption observations that precede a recovery check.
    ///
    /// The live-mailbox owner must obtain this token BEFORE its full integrity
    /// probe and complete it only after that probe and its required follow-up
    /// checks pass. Quick checks, diagnostic copies, and decoded result rows
    /// alone cannot authorize recovery. Any newer corruption observation
    /// invalidates the token, even while the breaker is already open.
    pub fn begin_recovery_check(&self) -> CorruptionRecoveryCheck<'_> {
        CorruptionRecoveryCheck {
            breaker: self,
            observed_trip_count: self.lock_state().trip_count,
        }
    }

    /// Explicit operator/test reset. Automatic recovery must instead use
    /// [`Self::begin_recovery_check`] so stale evidence cannot reopen writes.
    /// The lifetime observation count is retained.
    pub fn reset(&self) {
        let mut state = self.lock_state();
        state.detail = None;
        self.tripped.store(false, Ordering::Release);
        // Keep clearing evidence and publishing the closed flag serialized.
        drop(state);
    }

    /// Robot/health-facing snapshot of one coherent breaker state.
    #[must_use]
    pub fn snapshot(&self) -> CorruptionBreakerSnapshot {
        let state = self.lock_state();
        CorruptionBreakerSnapshot {
            tripped: state.detail.is_some(),
            trip_count: state.trip_count,
            tripped_at_us: state.detail.as_ref().map_or(0, |d| d.tripped_at_us),
            class: state.detail.as_ref().map(|d| d.class.as_str().to_string()),
        }
    }
}

/// Single-use evidence token tied to the breaker that issued it.
///
/// Dropping the token does nothing: a failed, cancelled, or abandoned check
/// never clears the breaker. The token intentionally is neither `Copy` nor
/// `Clone`, and its private fields prevent callers from fabricating an epoch.
#[derive(Debug)]
#[must_use = "complete only after a successful full check of the live mailbox"]
pub struct CorruptionRecoveryCheck<'a> {
    breaker: &'a CorruptionCircuitBreaker,
    observed_trip_count: u64,
}

impl CorruptionRecoveryCheck<'_> {
    /// Clear this breaker's evidence only if no corruption has been observed
    /// since the token was issued. The caller is responsible for establishing
    /// a successful full check of the live mailbox before calling this method.
    ///
    /// Returns false when the evidence is stale. A saturated lifetime counter
    /// also refuses automatic recovery: equality can no longer prove that no
    /// newer observation occurred. Explicit operator reset remains available.
    #[must_use]
    pub fn reset_if_unchanged(self) -> bool {
        let mut state = self.breaker.lock_state();
        if state.trip_count == u64::MAX || state.trip_count != self.observed_trip_count {
            return false;
        }
        state.detail = None;
        self.breaker.tripped.store(false, Ordering::Release);
        drop(state);
        true
    }
}

/// Serializable snapshot for health/robot surfaces.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorruptionBreakerSnapshot {
    pub tripped: bool,
    pub trip_count: u64,
    pub tripped_at_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

static BREAKER: OnceLock<CorruptionCircuitBreaker> = OnceLock::new();

/// The process-global corruption circuit breaker.
#[must_use]
pub fn corruption_circuit_breaker() -> &'static CorruptionCircuitBreaker {
    BREAKER.get_or_init(CorruptionCircuitBreaker::default)
}

/// Explicitly clear the process-global corruption circuit breaker.
/// Automatic recovery must use a token from `begin_recovery_check` and
/// complete it only after full verification of the live mailbox.
pub fn reset_corruption_circuit_breaker() {
    corruption_circuit_breaker().reset();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn verified_recovery_preserves_lifetime_observations() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "original damage");
        assert!(breaker.begin_recovery_check().reset_if_unchanged());
        assert!(!breaker.is_tripped());
        assert!(breaker.refusal_error().is_none());
        assert_eq!(breaker.trip_count(), 1);
    }

    #[test]
    fn recovery_cannot_clear_an_observation_after_the_probe_started() {
        let breaker = CorruptionCircuitBreaker::default();
        let check = breaker.begin_recovery_check();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "new damage");
        assert!(!check.reset_if_unchanged());
        assert!(breaker.is_tripped());
        assert!(
            breaker
                .refusal_error()
                .unwrap()
                .to_string()
                .contains("new damage")
        );
    }

    #[test]
    fn repeated_damage_invalidates_an_open_epoch_recovery() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "first damage");
        let check = breaker.begin_recovery_check();
        breaker.trip(DbErrorClass::WalSidecarCorruption, "later damage");
        assert!(!check.reset_if_unchanged());
        assert!(breaker.is_tripped());
        assert_eq!(breaker.trip_count(), 2);
        assert!(
            breaker
                .refusal_error()
                .unwrap()
                .to_string()
                .contains("first damage")
        );
    }

    #[test]
    fn recovery_cannot_clear_a_reset_and_retrip_epoch() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "old damage");
        let check = breaker.begin_recovery_check();
        breaker.reset();
        breaker.trip(DbErrorClass::WalSidecarCorruption, "new epoch");
        assert!(!check.reset_if_unchanged());
        assert_eq!(
            breaker.snapshot().class.as_deref(),
            Some("wal_sidecar_corruption")
        );
    }

    #[test]
    fn abandoning_recovery_does_not_release_writes() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "unrepaired damage");
        {
            let _check = breaker.begin_recovery_check();
        }
        assert!(breaker.is_tripped());
        assert_eq!(breaker.trip_count(), 1);
    }

    #[test]
    fn saturated_observation_counter_refuses_automatic_recovery() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "damage");
        breaker.lock_state().trip_count = u64::MAX;
        let check = breaker.begin_recovery_check();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "untrackable newer damage");
        assert!(!check.reset_if_unchanged());
        assert!(breaker.is_tripped());
    }

    #[test]
    fn concurrent_corruption_invalidates_an_in_flight_full_probe() {
        let breaker = CorruptionCircuitBreaker::default();
        breaker.trip(DbErrorClass::MainDbBtreeCorruption, "original damage");
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (finish_tx, finish_rx) = mpsc::channel();
            let breaker_ref = &breaker;
            let worker = scope.spawn(move || {
                let check = breaker_ref.begin_recovery_check();
                started_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                check.reset_if_unchanged()
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            breaker.trip(DbErrorClass::WalSidecarCorruption, "concurrent damage");
            finish_tx.send(()).unwrap();
            assert!(!worker.join().unwrap());
        });
        assert!(breaker.is_tripped());
        assert_eq!(breaker.trip_count(), 2);
    }

    #[test]
    fn trips_refuses_and_resets() {
        let b = CorruptionCircuitBreaker::default();
        assert!(!b.is_tripped());
        assert!(b.refusal_error().is_none());

        b.trip(
            DbErrorClass::MainDbBtreeCorruption,
            "SQLite error: database disk image is malformed",
        );
        assert!(b.is_tripped());

        let refusal = b.refusal_error().expect("open breaker must refuse writes");
        // The refusal must re-classify as corruption (keywords preserved) and
        // carry degraded-mode guidance — never a generic/scary raw error.
        assert!(
            refusal.is_corruption(),
            "refusal must be a corruption class"
        );
        assert!(refusal.classification().blocks_edits);
        let text = refusal.to_string();
        assert!(text.contains("am doctor repair"));
        assert!(text.contains("Reads remain available"));

        b.reset();
        assert!(!b.is_tripped());
        assert!(b.refusal_error().is_none());
    }

    #[test]
    fn observe_error_only_trips_on_edit_blocking_corruption() {
        let b = CorruptionCircuitBreaker::default();

        // Transient contention must NOT trip the corruption breaker.
        assert!(!b.observe_error(&DbError::ResourceBusy("database is locked".into())));
        assert!(!b.observe_error(&DbError::Pool("pool exhausted".into())));
        assert!(!b.is_tripped());

        // Hard corruption trips it.
        assert!(b.observe_error(&DbError::Sqlite("database disk image is malformed".into())));
        assert!(b.is_tripped());
    }

    #[test]
    fn trip_is_idempotent_keeps_first_cause_and_counts() {
        let b = CorruptionCircuitBreaker::default();
        b.trip(DbErrorClass::WalSidecarCorruption, "wal sidecar corrupt");
        b.trip(DbErrorClass::MainDbBtreeCorruption, "later btree damage");
        assert_eq!(b.trip_count(), 2);
        let snap = b.snapshot();
        assert!(snap.tripped);
        assert_eq!(snap.trip_count, 2);
        // First cause is preserved.
        assert_eq!(snap.class.as_deref(), Some("wal_sidecar_corruption"));
    }

    struct DelayedMessage {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl From<DelayedMessage> for String {
        fn from(message: DelayedMessage) -> Self {
            message.entered.send(()).expect("announce conversion");
            message
                .release
                .recv_timeout(Duration::from_secs(10))
                .expect("release delayed conversion");
            "old database disk image is malformed".to_string()
        }
    }

    #[test]
    fn delayed_trip_cannot_overwrite_cause_after_reset_and_retrip() {
        let b = CorruptionCircuitBreaker::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let delayed = scope.spawn(|| {
                b.trip(
                    DbErrorClass::MainDbBtreeCorruption,
                    DelayedMessage {
                        entered: entered_tx,
                        release: release_rx,
                    },
                );
            });
            entered_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("first trip reached conversion");
            // This interleaving used to leave the new open flag paired with
            // the old delayed trip's class/message/timestamp.
            b.reset();
            b.trip(
                DbErrorClass::WalSidecarCorruption,
                "new wal sidecar corrupt",
            );
            release_tx.send(()).expect("resume first trip");
            delayed.join().expect("delayed trip completed");
        });

        let snapshot = b.snapshot();
        assert!(snapshot.tripped);
        assert_eq!(snapshot.trip_count, 2);
        assert_eq!(snapshot.class.as_deref(), Some("wal_sidecar_corruption"));
        let refusal = b.refusal_error().expect("new epoch remains open");
        assert!(refusal.to_string().contains("new wal sidecar corrupt"));
        assert!(!refusal.to_string().contains("old database"));
    }

    #[test]
    fn reset_clears_epoch_evidence_but_preserves_lifetime_count() {
        let b = CorruptionCircuitBreaker::default();
        b.trip(DbErrorClass::WalSidecarCorruption, "wal sidecar corrupt");
        b.reset();
        let closed = b.snapshot();
        assert!(!closed.tripped);
        assert_eq!(closed.class, None);
        assert_eq!(closed.tripped_at_us, 0);
        assert_eq!(closed.trip_count, 1);

        b.trip(
            DbErrorClass::MainDbBtreeCorruption,
            "database disk image is malformed",
        );
        let reopened = b.snapshot();
        assert!(reopened.tripped);
        assert_eq!(reopened.trip_count, 2);
        assert_eq!(reopened.class.as_deref(), Some("main_db_btree_corruption"));
    }

    #[test]
    fn poisoned_state_preserves_refusal_evidence() {
        let b = CorruptionCircuitBreaker::default();
        b.trip(
            DbErrorClass::WalSidecarCorruption,
            "original wal sidecar corrupt",
        );
        let interrupted = std::panic::catch_unwind(|| {
            let _guard = b.state.lock().expect("initial state lock");
            panic!("simulate an interrupted state observer");
        });
        assert!(interrupted.is_err());
        assert!(b.state.is_poisoned());
        let refusal = b.refusal_error().expect("poison cannot reopen writes");
        assert!(refusal.to_string().contains("original wal sidecar corrupt"));
        assert_eq!(
            b.snapshot().class.as_deref(),
            Some("wal_sidecar_corruption")
        );
        b.reset();
        assert!(b.refusal_error().is_none());
        b.trip(
            DbErrorClass::MainDbBtreeCorruption,
            "database disk image is malformed",
        );
        assert!(
            b.refusal_error()
                .expect("retrip after poison")
                .is_corruption()
        );
    }

    #[test]
    fn trip_counter_saturates_instead_of_wrapping() {
        let b = CorruptionCircuitBreaker::default();
        b.lock_state().trip_count = u64::MAX;
        b.trip(DbErrorClass::WalSidecarCorruption, "wal sidecar corrupt");
        assert_eq!(b.trip_count(), u64::MAX);
        assert!(b.is_tripped());
    }

    #[test]
    fn concurrent_transition_snapshots_never_mix_open_and_closed_evidence() {
        const ROUNDS: u64 = 512;
        let b = CorruptionCircuitBreaker::default();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                start.wait();
                for _ in 0..ROUNDS {
                    b.trip(DbErrorClass::WalSidecarCorruption, "wal sidecar corrupt");
                    std::thread::yield_now();
                    b.reset();
                    std::thread::yield_now();
                }
            });
            start.wait();
            for _ in 0..ROUNDS * 2 {
                let snapshot = b.snapshot();
                assert_eq!(snapshot.tripped, snapshot.class.is_some());
                assert!(snapshot.trip_count <= ROUNDS);
                if !snapshot.tripped {
                    assert_eq!(snapshot.tripped_at_us, 0);
                }
                std::thread::yield_now();
            }
            writer.join().expect("transition writer completed");
        });
        assert!(!b.is_tripped());
        assert_eq!(b.trip_count(), ROUNDS);
    }

    #[test]
    fn cursor_probe_limitation_does_not_trip_but_distinct_corruption_does() {
        let cursor = "database disk image is malformed: table_seek called on index page (type LeafIndex, page 1560, root 22): cursor is_table flag likely incorrect";
        for message in [
            cursor,
            "database disk image is malformed: index_seek called on table page (type LeafTable, page 1560, root 22): cursor is_table flag likely incorrect",
        ] {
            let breaker = CorruptionCircuitBreaker::default();
            assert!(!breaker.observe_error(&DbError::Sqlite(message.to_string())));
            assert!(!breaker.is_tripped());
        }
        for message in [
            "database disk image is malformed".to_string(),
            format!(
                "{cursor}; page 7 is referenced multiple times (freelist trunk[1] leaf[0]; index messages root -> child[0])"
            ),
            format!("{cursor}; database disk image is malformed: freelist mismatch"),
        ] {
            let breaker = CorruptionCircuitBreaker::default();
            assert!(breaker.observe_error(&DbError::Sqlite(message)));
            assert!(breaker.is_tripped());
        }
        let breaker = CorruptionCircuitBreaker::default();
        assert!(breaker.observe_error(&DbError::IntegrityCorruption {
            message: cursor.to_string(),
            details: Vec::new(),
        }));
        assert!(breaker.is_tripped());
    }
}
