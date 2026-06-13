//! Corruption-specific DB circuit breaker (br-bvq1x.11.3 / K3).
//!
//! Once a HARD, edit-blocking corruption-class error (A1) is observed on the
//! write path, this breaker trips and STAYS tripped: subsequent writes are
//! refused immediately — without touching the database again — so agents stop
//! hammering a corrupt store (re-emitting scary errors and risking worse
//! damage). The refusal carries the corruption keywords of the triggering
//! error, so it re-classifies as corruption and renders the structured A2
//! envelope plus `am doctor repair` / `reconstruct` guidance.
//!
//! This is deliberately distinct from the auto-recovering, time-reset
//! per-subsystem breakers in [`crate::retry`] (which are for transient
//! contention, classed `RESOURCE_BUSY`). Corruption must NOT auto-recover on a
//! timer; it clears only when the database is verified healthy again (a clean
//! integrity check calls [`reset_corruption_circuit_breaker`]) or the process
//! restarts.
//!
//! ## Scope: writes only, server only
//!
//! The breaker is checked solely inside `run_with_mvcc_retry` (the async
//! server write path), so reads (which do not go through it) are never
//! affected. It is also process-local: the CLI/doctor sync write path runs in a
//! separate process whose breaker is never tripped, so recovery via `am doctor`
//! is never blocked.

#![forbid(unsafe_code)]

use crate::error::{DbError, DbErrorClass};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

fn now_micros_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_micros()).ok())
        .unwrap_or(0)
}

/// Recorded when the breaker first trips. (The trip timestamp is held in the
/// breaker's `tripped_at_us` atomic.)
#[derive(Debug, Clone)]
struct TripDetail {
    class: DbErrorClass,
    message: String,
}

/// A sticky, corruption-only circuit breaker.
#[derive(Debug, Default)]
pub struct CorruptionCircuitBreaker {
    tripped: AtomicBool,
    tripped_at_us: AtomicU64,
    trip_count: AtomicU64,
    detail: Mutex<Option<TripDetail>>,
}

impl CorruptionCircuitBreaker {
    /// Trip the breaker for a hard corruption-class error. Idempotent: only the
    /// first trip records the triggering detail; later corruption errors bump
    /// the count but keep the original cause.
    pub fn trip(&self, class: DbErrorClass, message: impl Into<String>) {
        self.trip_count.fetch_add(1, Ordering::Relaxed);
        if !self.tripped.swap(true, Ordering::SeqCst) {
            let now = now_micros_u64();
            self.tripped_at_us.store(now, Ordering::Relaxed);
            if let Ok(mut guard) = self.detail.lock() {
                *guard = Some(TripDetail {
                    class,
                    message: message.into(),
                });
            }
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
        self.tripped.load(Ordering::SeqCst)
    }

    /// Total number of corruption errors observed since the last reset.
    #[must_use]
    pub fn trip_count(&self) -> u64 {
        self.trip_count.load(Ordering::Relaxed)
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
        let original = self
            .detail
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| d.message.clone()))
            .unwrap_or_else(|| "database corruption detected".to_string());
        Some(DbError::Sqlite(format!(
            "corruption circuit breaker open: refusing this write to avoid worsening damage to a \
             corrupt database. Reads remain available; run `am doctor repair` or `am doctor \
             reconstruct` to recover, then a clean integrity check (or a restart) clears the \
             breaker. Triggering error: {original}"
        )))
    }

    /// Clear the breaker. Called by a clean integrity check (self-heal) and
    /// available to operators/tests.
    pub fn reset(&self) {
        self.tripped.store(false, Ordering::SeqCst);
        self.tripped_at_us.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.detail.lock() {
            *guard = None;
        }
    }

    /// Robot/health-facing snapshot of the breaker state.
    #[must_use]
    pub fn snapshot(&self) -> CorruptionBreakerSnapshot {
        let class = self
            .detail
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| d.class.as_str().to_string()));
        CorruptionBreakerSnapshot {
            tripped: self.is_tripped(),
            trip_count: self.trip_count(),
            tripped_at_us: self.tripped_at_us.load(Ordering::Relaxed),
            class,
        }
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

/// Clear the process-global corruption circuit breaker (called when an
/// integrity check passes, and available to operators).
pub fn reset_corruption_circuit_breaker() {
    corruption_circuit_breaker().reset();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(refusal.is_corruption(), "refusal must be a corruption class");
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
        assert!(b.observe_error(&DbError::Sqlite(
            "database disk image is malformed".into()
        )));
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
}
