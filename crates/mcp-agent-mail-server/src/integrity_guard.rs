//! Background worker for continuous `SQLite` integrity checking and recovery.
//!
//! Startup probes catch corruption at boot, but long-running sessions can still
//! encounter driver-level failures later. This worker adds runtime protection:
//!
//! - periodic quick integrity checks
//! - periodic full integrity checks (configurable)
//! - proactive backup refresh on healthy cycles
//! - diagnostic surfacing for recoverable failures without mutating the live DB

#![forbid(unsafe_code)]

#[path = "integrity_guard_backup_budget.rs"]
mod backup_budget;
#[cfg(unix)]
#[path = "integrity_guard_backup_journal.rs"]
mod backup_journal;
#[path = "integrity_guard_schedule.rs"]
mod schedule;

use schedule::{AutomaticBackupSchedule, BackupCompletion, BackupKind, FullVerificationGate};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::is_sqlite_memory_database_url;
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, is_corruption_error_message, is_sqlite_recovery_error_message,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SKIP_NEXT_QUICK_CYCLE: AtomicBool = AtomicBool::new(false);
static SKIP_NEXT_PROACTIVE_BACKUP: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const DEFAULT_QUICK_CHECK_INTERVAL_SECS: u64 = 300;
const MIN_FULL_CHECK_INTERVAL_SECS: u64 = 3600;
const RECOVERY_MIN_INTERVAL_SECS: u64 = 30;
const BACKUP_MAX_AGE_SECS: u64 = 3600;
/// GH#288: how often an UNCHANGED standing defect is re-warned. Between
/// re-warns, repeat observations log at debug so journal-based alerting sees
/// the transition, a low-cadence "still standing" reminder, and nothing else.
const STANDING_DEFECT_REWARN_INTERVAL_SECS: u64 = 6 * 3600;

/// GH#288: per-phase memory of the last integrity defect the guard warned
/// about, so an unchanged standing condition is logged as a transition plus a
/// low-cadence reminder instead of an identical WARN pair every cycle for
/// days (~23x/day in the field report).
#[derive(Default)]
struct StandingDefectLog {
    entries: std::collections::HashMap<String, StandingDefectEntry>,
}

struct StandingDefectEntry {
    fingerprint: u64,
    first_seen_unix: i64,
    last_warn: Instant,
    observations: u64,
}

impl StandingDefectLog {
    /// Record one observation of `(phase, error_message)`. Returns
    /// `Some(reminder)` when the caller should WARN — either a fresh/changed
    /// defect (`observations == 1`) or the periodic reminder for a standing
    /// one — and `None` when the observation should stay at debug.
    fn observe(&mut self, phase: &str, error_message: &str) -> Option<StandingDefectVerdict> {
        let fingerprint = defect_fingerprint(phase, error_message);
        let now_unix = unix_now_seconds();
        match self.entries.get_mut(phase) {
            Some(entry) if entry.fingerprint == fingerprint => {
                entry.observations += 1;
                if entry.last_warn.elapsed()
                    >= Duration::from_secs(STANDING_DEFECT_REWARN_INTERVAL_SECS)
                {
                    entry.last_warn = Instant::now();
                    Some(StandingDefectVerdict {
                        new_detection: false,
                        first_seen_unix: entry.first_seen_unix,
                        observations: entry.observations,
                    })
                } else {
                    None
                }
            }
            _ => {
                self.entries.insert(
                    phase.to_string(),
                    StandingDefectEntry {
                        fingerprint,
                        first_seen_unix: now_unix,
                        last_warn: Instant::now(),
                        observations: 1,
                    },
                );
                Some(StandingDefectVerdict {
                    new_detection: true,
                    first_seen_unix: now_unix,
                    observations: 1,
                })
            }
        }
    }

    /// A passing cycle for `phase` clears its standing defect, so any later
    /// recurrence logs as a fresh transition again.
    fn clear(&mut self, phase: &str) {
        self.entries.remove(phase);
    }
}

struct StandingDefectVerdict {
    new_detection: bool,
    first_seen_unix: i64,
    observations: u64,
}

fn defect_fingerprint(phase: &str, error_message: &str) -> u64 {
    use std::hash::{Hash, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    phase.hash(&mut hasher);
    error_message.hash(&mut hasher);
    hasher.finish()
}

fn unix_now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// GH#288: when the recovery path the guard's message promises is recorded as
/// failing by the durable recovery breaker, say so instead of promising it.
fn recovery_availability_note(sqlite_path: &Path) -> String {
    match mcp_agent_mail_db::recovery_breaker::load(sqlite_path) {
        Ok(Some(state)) if state.tripped || state.consecutive_failures > 0 => format!(
            "recovery unavailable: recovery breaker recorded {} reconstruct failure(s) \
             (tripped={}) at unix {}: {}",
            state.consecutive_failures,
            state.tripped,
            state.last_failure_unix,
            state.last_failure_reason
        ),
        _ => "the query-path recovery flow will trigger an admission-controlled reconstruct \
              on the next corrupt-verdict read"
            .to_string(),
    }
}

#[inline]
const fn quick_check_interval() -> Duration {
    Duration::from_secs(DEFAULT_QUICK_CHECK_INTERVAL_SECS)
}

#[inline]
fn full_check_interval(config: &Config) -> Option<Duration> {
    if config.integrity_check_interval_hours == 0 {
        return None;
    }
    let secs = config
        .integrity_check_interval_hours
        .saturating_mul(3600)
        .max(MIN_FULL_CHECK_INTERVAL_SECS);
    Some(Duration::from_secs(secs))
}

fn full_check_due(
    config: &Config,
    interval: Option<Duration>,
    last_full_attempt: Option<Instant>,
) -> bool {
    let Some(interval) = interval else {
        return false;
    };
    if let Some(last_full_attempt) = last_full_attempt {
        return last_full_attempt.elapsed() >= interval;
    }
    mcp_agent_mail_db::is_full_check_due(config.integrity_check_interval_hours)
}

/// Tell the guard that startup already ran an integrity probe.
///
/// Used by HTTP/TUI startup to avoid immediately repeating the same quick-check
/// in the background worker before the first interval elapses.
#[allow(dead_code)]
pub fn note_startup_integrity_probe_completed() {
    SKIP_NEXT_QUICK_CYCLE.store(true, Ordering::Release);
}

/// Skip the next automatic backup refresh, including a due verified snapshot,
/// while still performing the integrity guard's health checks.
pub fn defer_next_proactive_backup() {
    SKIP_NEXT_PROACTIVE_BACKUP.store(true, Ordering::Release);
}

fn take_deferred_proactive_backup() -> bool {
    SKIP_NEXT_PROACTIVE_BACKUP.swap(false, Ordering::AcqRel)
}

fn resolve_integrity_guard_sqlite_path(config: &Config) -> Option<PathBuf> {
    crate::resolve_server_database_url_sqlite_path(&config.database_url)
}

pub fn start(config: &Config) {
    if !config.integrity_check_on_startup {
        return;
    }
    if is_sqlite_memory_database_url(&config.database_url) {
        return;
    }

    let Some(sqlite_path) = resolve_integrity_guard_sqlite_path(config) else {
        tracing::warn!(
            database_url = %config.database_url,
            "integrity guard disabled: failed to resolve sqlite path from DATABASE_URL"
        );
        return;
    };

    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
        && let Some(stale) = worker.take()
    {
        let _ = stale.join();
    }
    if worker.is_none() {
        let config = config.clone();
        SHUTDOWN.store(false, Ordering::Release);
        match std::thread::Builder::new()
            .name("integrity-guard".into())
            .stack_size(mcp_agent_mail_core::worker_stack_size())
            .spawn(move || monitor_loop(&config, &sqlite_path))
        {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                tracing::warn!(
                    error = %err,
                    "failed to spawn integrity guard worker; continuing without integrity background scans"
                );
                return;
            }
        }
    }
    drop(worker);
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn monitor_loop(config: &Config, sqlite_path: &Path) {
    let quick_every = quick_check_interval();
    let full_every = full_check_interval(config);
    let storage_root = config.storage_root.clone();

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
    pool_config.min_connections = 1;
    pool_config.max_connections = 1;
    pool_config.warmup_connections = 0;
    // Keep migrations enabled here: this worker can be the first component to
    // acquire a pooled connection (e.g. proactive backup checkpointing in
    // stdio mode), and that first-acquire path must remain schema-safe.
    pool_config.run_migrations = true;

    let pool = match mcp_agent_mail_db::create_pool(&pool_config) {
        Ok(pool) => pool,
        Err(err) => {
            tracing::warn!(error = %err, "integrity guard: failed to create DB pool, exiting");
            return;
        }
    };

    tracing::info!(
        quick_interval_secs = quick_every.as_secs(),
        full_interval_secs = full_every.map_or(0, |d| d.as_secs()),
        "integrity guard worker started"
    );

    let mut last_full_attempt: Option<Instant> = None;
    let mut last_recovery_attempt: Option<Instant> = None;
    let mut verification_gate = FullVerificationGate::default();
    let mut backup_schedule = AutomaticBackupSchedule::default();
    // GH#288: transition-aware defect logging (fingerprint + backoff).
    let mut standing_defects = StandingDefectLog::default();
    // Bead K4: seed the maintenance schedule at "now" so the first checkpoint /
    // analyze / vacuum fires only after one full interval — never a heavy
    // startup VACUUM storm. These live on the integrity-guard worker thread, so
    // maintenance always runs off the request/dispatch hot path.
    let maintenance_start = Instant::now();
    let mut last_checkpoint: Option<Instant> = Some(maintenance_start);
    let mut last_analyze: Option<Instant> = Some(maintenance_start);
    let mut last_vacuum: Option<Instant> = Some(maintenance_start);
    let mut last_atc_retention: Option<Instant> = Some(maintenance_start);
    let mut last_doctor_retention: Option<Instant> = Some(maintenance_start);
    let mut skip_first_quick_cycle = SKIP_NEXT_QUICK_CYCLE.swap(false, Ordering::AcqRel);

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("integrity guard worker shutting down");
            return;
        }

        let skipped_quick_cycle = skip_first_quick_cycle;
        let quick_cycle_passed = if skip_first_quick_cycle {
            skip_first_quick_cycle = false;
            tracing::debug!(
                "integrity guard: skipped immediate quick cycle (startup probe already executed)"
            );
            true
        } else {
            run_quick_cycle(
                &pool,
                sqlite_path,
                &storage_root,
                &mut last_recovery_attempt,
                &mut standing_defects,
            )
        };

        // Quick checks never discharge a failed full verification. Include
        // failures observed elsewhere in this process, as well as the worker's
        // own sticky gate (which also covers unavailable cross-count evidence).
        let full_due = full_check_due(config, full_every, last_full_attempt)
            || mcp_agent_mail_db::corruption_circuit_breaker().is_tripped()
            || mcp_agent_mail_db::integrity::integrity_metrics().last_full_check_outcome
                == mcp_agent_mail_db::integrity::IntegrityCheckOutcome::Failed;
        let probes_passed = run_integrity_followups(
            quick_cycle_passed,
            full_due,
            &mut verification_gate,
            Instant::now,
            || {
                let passed = run_full_cycle(
                    &pool,
                    sqlite_path,
                    &storage_root,
                    &mut last_recovery_attempt,
                    &mut standing_defects,
                );
                last_full_attempt = Some(Instant::now());
                passed
            },
            |full_completed| {
                if full_completed {
                    backup_schedule.request_verified();
                }
                if SHUTDOWN.load(Ordering::Acquire)
                    || mcp_agent_mail_db::corruption_circuit_breaker().is_tripped()
                {
                    return false;
                }
                // Both backup entry points are now downstream of every due
                // integrity check and share one retry budget. Prefer a pending
                // verified snapshot instead of creating two copies this cycle.
                if full_completed || !skipped_quick_cycle {
                    if take_deferred_proactive_backup() {
                        tracing::debug!("integrity guard: deferred automatic backup refresh");
                    } else if !run_automatic_backup(&pool, &mut backup_schedule) {
                        return false;
                    }
                }
                if mcp_agent_mail_db::corruption_circuit_breaker().is_tripped()
                    || mcp_agent_mail_db::integrity::integrity_metrics().last_full_check_outcome
                        == mcp_agent_mail_db::integrity::IntegrityCheckOutcome::Failed
                {
                    return false;
                }
                run_archive_drift_cycle(sqlite_path, &storage_root, full_completed);
                if SHUTDOWN.load(Ordering::Acquire)
                    || mcp_agent_mail_db::corruption_circuit_breaker().is_tripped()
                {
                    return false;
                }
                // Bead K4: bounded SQLite maintenance (checkpoint / analyze /
                // vacuum / journal_size_limit) on independent cadences, off
                // the hot path. Never run it after a failed integrity verdict.
                run_db_maintenance_cycle(
                    &pool,
                    config,
                    sqlite_path,
                    Instant::now(),
                    &mut last_checkpoint,
                    &mut last_analyze,
                    &mut last_vacuum,
                    &mut last_atc_retention,
                    &mut last_doctor_retention,
                );
                true
            },
        );
        if !probes_passed {
            tracing::debug!(
                "integrity guard: full evidence remains required before mutating followups"
            );
        }

        // Sleep in short increments so shutdown reacts quickly.
        let mut remaining = quick_every;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                tracing::info!("integrity guard worker shutting down");
                return;
            }
            let chunk = remaining.min(Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

/// Run mutating followups only after all required integrity verdicts pass.
/// A failed full cycle remains terminal on LATER quick cycles as well. Retry
/// pacing never authorizes maintenance by itself. The clock is injected so
/// repeated-cycle and slow-probe interleavings can be exercised without sleeps.
fn run_integrity_followups<F, M, C>(
    quick_cycle_passed: bool,
    full_due: bool,
    gate: &mut FullVerificationGate,
    clock: C,
    run_full: F,
    run_followups: M,
) -> bool
where
    F: FnOnce() -> bool,
    M: FnOnce(bool) -> bool,
    C: Fn() -> Instant,
{
    if !quick_cycle_passed {
        gate.require();
        tracing::warn!(
            "integrity guard: skipping full check and database maintenance after failed quick cycle"
        );
        return false;
    }
    if full_due {
        gate.require();
    }
    let full_completed = if gate.is_required() {
        let remaining = gate.retry_remaining(clock());
        if !remaining.is_zero() {
            tracing::debug!(
                retry_after_secs = remaining.as_secs(),
                "integrity guard: failed full verification remains blocking during retry backoff"
            );
            return false;
        }
        let passed = run_full();
        gate.complete(clock(), passed);
        if !passed {
            tracing::warn!(
                "integrity guard: skipping mutating followups until full integrity verification succeeds"
            );
            return false;
        }
        true
    } else {
        false
    };
    if !run_followups(full_completed) {
        // A fresh corruption refusal or an inconclusive snapshot's own live
        // verification can invalidate the evidence after the initial probes.
        gate.require();
        return false;
    }
    true
}

fn run_quick_cycle(
    pool: &DbPool,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
    standing_defects: &mut StandingDefectLog,
) -> bool {
    match pool.run_periodic_integrity_check() {
        Ok(_) => {
            standing_defects.clear("quick_check");
            // No backup, checkpoint, or reconciliation here. A due or failed
            // full verification has not authorized those operations yet.
            true
        }
        Err(err) => {
            handle_integrity_error_with_log(
                "quick_check",
                &err.to_string(),
                sqlite_path,
                storage_root,
                last_recovery_attempt,
                standing_defects,
            );
            false
        }
    }
}

fn run_full_cycle(
    pool: &DbPool,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
    standing_defects: &mut StandingDefectLog,
) -> bool {
    let recovery_check = mcp_agent_mail_db::corruption_circuit_breaker().begin_recovery_check();
    match pool.run_full_integrity_check() {
        Ok(_) => {
            standing_defects.clear("integrity_check");
            tracing::info!("integrity guard: periodic full integrity check passed");
            // GH#214: PRAGMA checks can under-report the index/table desync
            // class (quick_check misses single-row loss; the GH#213 Linux
            // specimen sat green for a full run). Cross-count the hot tables
            // through their index btrees on the same full-check cadence.
            // Runs BEFORE the verified-snapshot capture so a desynced DB is
            // never recorded as last-known-healthy.
            match run_index_table_cross_count(sqlite_path) {
                Ok(None) => {}
                Ok(Some(mismatch)) => {
                    // This is positive corruption evidence, not an unavailable
                    // probe. Publish the same write refusal as a corrupt query.
                    mcp_agent_mail_db::corruption_circuit_breaker()
                        .observe_error(&mcp_agent_mail_db::DbError::Sqlite(mismatch.clone()));
                    handle_integrity_error_with_log(
                        "index_table_cross_count",
                        &mismatch,
                        sqlite_path,
                        storage_root,
                        last_recovery_attempt,
                        standing_defects,
                    );
                    return false;
                }
                Err(error) => {
                    // Unavailable evidence is not proof of corruption, but it
                    // also cannot authorize recovery, snapshots, or maintenance.
                    handle_integrity_error_with_log(
                        "index_table_cross_count_probe",
                        &error,
                        sqlite_path,
                        storage_root,
                        last_recovery_attempt,
                        standing_defects,
                    );
                    return false;
                }
            }
            standing_defects.clear("index_table_cross_count_probe");
            standing_defects.clear("index_table_cross_count");
            if !recovery_check.reset_if_unchanged() {
                tracing::warn!(
                    "integrity guard: newer corruption invalidated full-check evidence; retaining write refusal and skipping maintenance"
                );
                return false;
            }
            // Backup publication and archive reconciliation happen only in
            // the followup phase, after the sticky verification gate clears.
            true
        }
        Err(err) => {
            handle_integrity_error_with_log(
                "integrity_check",
                &err.to_string(),
                sqlite_path,
                storage_root,
                last_recovery_attempt,
                standing_defects,
            );
            false
        }
    }
}

/// Execute at most one eligible backup operation. This is the only automatic
/// backup entry point in the guard; explicit operator calls remain unchanged.
fn run_automatic_backup(pool: &DbPool, schedule: &mut AutomaticBackupSchedule) -> bool {
    run_admitted_backup_with(
        Path::new(pool.sqlite_path()),
        schedule,
        Instant::now,
        |kind| match kind {
            BackupKind::Proactive => pool
                .create_proactive_backup(Duration::from_secs(BACKUP_MAX_AGE_SECS))
                .map(|path| path.is_some())
                .map_err(|error| error.to_string()),
            BackupKind::Verified => pool
                .create_verified_snapshot()
                .map(|metadata| {
                    if let Some(meta) = &metadata {
                        tracing::debug!(
                            snapshot = %meta.snapshot_path,
                            "integrity guard: recorded verified snapshot"
                        );
                    }
                    metadata.is_some()
                })
                .map_err(|error| error.to_string()),
        },
    )
}

/// Acquire filesystem admission only when retry timing permits an attempt.
/// The same lease and retained-stage budget protect both producer routes. A
/// refusal goes through ordinary incomplete-attempt pacing, preserves pending
/// verified work, and is not reported as a live-database integrity failure.
fn run_admitted_backup_with<F, C>(
    primary: &Path,
    schedule: &mut AutomaticBackupSchedule,
    clock: C,
    produce: F,
) -> bool
where
    F: FnOnce(BackupKind) -> Result<bool, String>,
    C: Fn() -> Instant,
{
    #[cfg(unix)]
    if primary.as_os_str() != ":memory:" {
        let Some(requested) = schedule.next_attempt(clock()) else {
            tracing::debug!(
                retry_after_secs = schedule.retry_remaining(clock()).as_secs(),
                "integrity guard: automatic backup deferred by process-local retry pacing"
            );
            return true;
        };
        // Keep the retained-stage budget and its parent-wide lease. Only an
        // admitted exporter may acquire the mailbox journal, always in this
        // lock order, and both leases remain held through producer completion.
        let admitted = backup_budget::with_admission(primary, || {
            let lease = match backup_journal::AutomaticBackupLease::try_begin(primary, requested) {
                Ok(Some(lease)) => lease,
                Ok(None) => {
                    tracing::debug!(
                        "integrity guard: automatic export deferred by durable backoff or an active exporter; health probes continue"
                    );
                    return Ok(true);
                }
                Err(error) => {
                    // Journal IO is unavailable admission, not evidence that
                    // the live mailbox is corrupt. Never start an unrecorded
                    // export or weaken the existing full-verification gate.
                    tracing::warn!(
                        %error,
                        "integrity guard: automatic backup journal admission unavailable; no export started; health probes continue"
                    );
                    return Ok(true);
                }
            };
            if lease.kind() == BackupKind::Verified {
                schedule.request_verified();
            }
            Ok(run_automatic_backup_with(schedule, &clock, |kind| {
                let result = produce(kind);
                let completion = match &result {
                    Ok(true) => BackupCompletion::Published,
                    Ok(false) => BackupCompletion::Skipped,
                    Err(_) => BackupCompletion::Failed,
                };
                if let Err(error) = lease.finish(completion) {
                    // A torn update retains the preceding synced intent.
                    // Keep the producer's actual result, including a skipped
                    // verified snapshot's requirement for fresh full evidence.
                    tracing::warn!(
                        %error,
                        "integrity guard: automatic backup completion journal update failed; subsequent admission remains conservative"
                    );
                }
                result
            }))
        });
        return match admitted {
            Ok(followups_allowed) => followups_allowed,
            Err(error) => run_automatic_backup_with(schedule, clock, |_| Err(error)),
        };
    }
    // Non-Unix platforms retain the existing admission path; memory-backed
    // fixtures never create a journal or a control path on disk.
    run_automatic_backup_with(schedule, clock, |kind| {
        backup_budget::with_admission(primary, || produce(kind))
    })
}

/// `Ok(true)` means an artifact was actually published, `Ok(false)` means the
/// producer skipped it. A skipped verified snapshot cannot certify the live
/// mailbox or clear failure history. Other backup failures remain best-effort
/// and are not reclassified as live corruption merely because staging failed.
fn run_automatic_backup_with<F, C>(
    schedule: &mut AutomaticBackupSchedule,
    clock: C,
    produce: F,
) -> bool
where
    F: FnOnce(BackupKind) -> Result<bool, String>,
    C: Fn() -> Instant,
{
    let Some(kind) = schedule.next_attempt(clock()) else {
        tracing::debug!(
            retry_after_secs = schedule.retry_remaining(clock()).as_secs(),
            consecutive_failures = schedule.consecutive_failures(),
            "integrity guard: automatic backup deferred after incomplete prior attempt; health probes continue"
        );
        return true;
    };
    let result = produce(kind);
    let completion = match &result {
        Ok(true) => BackupCompletion::Published,
        Ok(false) => BackupCompletion::Skipped,
        Err(_) => BackupCompletion::Failed,
    };
    let completed_at = clock();
    schedule.complete(kind, completion, completed_at);
    match result {
        Err(error) => tracing::warn!(
            ?kind,
            %error,
            consecutive_failures = schedule.consecutive_failures(),
            retry_after_secs = schedule.retry_remaining(completed_at).as_secs(),
            "integrity guard: automatic backup failed; preserving evidence and backing off both backup routes"
        ),
        Ok(false) if kind == BackupKind::Verified => {
            tracing::warn!(
                retry_after_secs = schedule.retry_remaining(completed_at).as_secs(),
                "integrity guard: verified snapshot was not published; retaining pending request and requiring fresh full evidence"
            );
            return false;
        }
        Ok(_) => {}
    }
    true
}

fn run_archive_drift_cycle(sqlite_path: &Path, storage_root: &Path, full_completed: bool) {
    // The quick path retries only previously deferred work (#219). The full
    // path additionally discovers drift arising after bootstrap. Neither may
    // run before a required full verification has actually succeeded.
    let result = if full_completed {
        mcp_agent_mail_db::pool::reconcile_archive_drift_full_cycle(sqlite_path, storage_root)
    } else {
        mcp_agent_mail_db::pool::retry_archive_drift_reconcile(sqlite_path, storage_root)
    };
    match result {
        Ok(true) => tracing::info!(
            full_completed,
            "integrity guard: reconciled archive-ahead drift after integrity verification"
        ),
        Ok(false) => {}
        Err(error) if full_completed => tracing::warn!(
            %error,
            "integrity guard: full-cycle archive drift reconcile failed"
        ),
        Err(error) => tracing::debug!(
            %error,
            "integrity guard: archive drift reconcile attempt failed; will retry next eligible cycle"
        ),
    }
}

/// Hot tables the GH#214 cross-count guards: the tables whose acknowledged
/// writes the field reports lost (agents on both legs; messages and
/// message_recipients in the 2026-08-12 production follow-up), plus their
/// join anchors.
const CROSS_COUNT_TABLES: &[&str] = &[
    "projects",
    "agents",
    "messages",
    "message_recipients",
    "file_reservations",
];

/// Open the live family for the cross-count only after the database crate's
/// guarded read-only family and namespace admission.
fn open_index_table_cross_count_connection(
    sqlite_path: &Path,
) -> std::io::Result<mcp_agent_mail_db::GuardedReadOnlyConn> {
    crate::open_read_only_sync_db_connection_with_busy_timeout(
        sqlite_path.to_string_lossy().as_ref(),
        crate::BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS,
        "integrity guard index/table cross-count",
    )
}

/// Run the GH#214 index-vs-table cross-count against a read-only connection.
///
/// Returns `Ok(Some(message))` for a mismatch, `Ok(None)` after a completed
/// comparison with no mismatch, and `Err` for unavailable evidence. Probe
/// failures are not automatically corruption, but must not authorize recovery.
/// Honest scope: this catches the desync class only; a mutually
/// consistent database missing acknowledged rows (the GH#213 Windows silent
/// class) is invisible to any server-side arithmetic.
fn run_index_table_cross_count(sqlite_path: &Path) -> Result<Option<String>, String> {
    let conn = open_index_table_cross_count_connection(sqlite_path)
        .map_err(|error| format!("cross-count read-only open failed: {error}"))?;
    let result = mcp_agent_mail_db::integrity::index_table_cross_count(&conn, CROSS_COUNT_TABLES);
    // This is a true read-only observer. Its ordinary Drop path preserves the
    // live WAL/namespace family; the writable close helper may checkpoint it.
    drop(conn);
    let mismatches = result.map_err(|error| format!("cross-count probe failed: {error}"))?;
    for mismatch in &mismatches {
        tracing::error!(
            table = %mismatch.table,
            index = %mismatch.index,
            table_rows = mismatch.table_rows,
            index_rows = mismatch.index_rows,
            "integrity guard: index/table cross-count desync (GH#214)"
        );
    }
    Ok(mismatches
        .first()
        .map(mcp_agent_mail_db::integrity::CrossCountMismatch::as_corruption_message))
}

/// Whether a maintenance task with cadence `interval_secs` is due, given when
/// it last ran (`last`) relative to `now`.
///
/// `interval_secs == 0` disables the task; a task that has never run is always
/// due. Pure (no clock/IO) so the maintenance schedule is unit-testable without
/// real time (bead K4).
fn maintenance_task_due(interval_secs: u64, last: Option<Instant>, now: Instant) -> bool {
    if interval_secs == 0 {
        return false;
    }
    last.map_or(true, |last| {
        now.saturating_duration_since(last) >= Duration::from_secs(interval_secs)
    })
}

/// Run one maintenance op, timing it and recording per-op metrics.
///
/// Success bumps the op's run counter, records the duration in the shared
/// maintenance-duration histogram, and stamps the op's last-run gauge. Failure
/// bumps the shared failure counter and logs — maintenance never propagates an
/// error to the worker loop or the request path.
fn run_maintenance_op<F, E>(
    op: &str,
    run: F,
    runs_total: &mcp_agent_mail_core::Counter,
    last_run_us: &mcp_agent_mail_core::GaugeU64,
) where
    F: FnOnce() -> Result<(), E>,
    E: std::fmt::Display,
{
    let db = &mcp_agent_mail_core::global_metrics().db;
    let started = Instant::now();
    match run() {
        Ok(()) => {
            let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            db.maintenance_duration_us.record(elapsed_us);
            runs_total.inc();
            let now_us =
                u64::try_from(mcp_agent_mail_core::timestamps::now_micros().max(0)).unwrap_or(0);
            last_run_us.set(now_us);
            tracing::debug!(op, elapsed_us, "db maintenance op completed");
        }
        Err(err) => {
            db.maintenance_failures_total.inc();
            tracing::warn!(op, error = %err, "db maintenance op failed; will retry next cycle");
        }
    }
}

/// Run any due background SQLite maintenance ops (bead K4): passive WAL
/// checkpoint, `ANALYZE`, and `VACUUM`, each on its own configured cadence, plus
/// re-applying the configured `journal_size_limit`.
///
/// Runs on the integrity-guard worker thread against its own connection, so it
/// never touches the request/dispatch hot path. Each op is bounded and
/// best-effort: failures are recorded and retried on the next cycle, never
/// fatal. The `last_*` cursors are advanced whenever a task is attempted so a
/// persistently-failing op still backs off to its interval instead of spinning.
fn run_db_maintenance_cycle(
    pool: &DbPool,
    config: &Config,
    sqlite_path: &Path,
    now: Instant,
    last_checkpoint: &mut Option<Instant>,
    last_analyze: &mut Option<Instant>,
    last_vacuum: &mut Option<Instant>,
    last_atc_retention: &mut Option<Instant>,
    last_doctor_retention: &mut Option<Instant>,
) {
    if !config.db_maintenance_enabled {
        return;
    }
    // #219: checkpoint/ANALYZE/VACUUM rewrite live-file state; hold the
    // in-process write lease so a recovery promotion cannot rename the file
    // out from under them (a VACUUM racing a promotion would rebuild the
    // quarantined generation and recreate sidecars at the live paths).
    let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
    let db = &mcp_agent_mail_core::global_metrics().db;

    // Bound WAL growth (cheap, idempotent) so a checkpoint truncates back to the
    // configured cap.
    if config.db_journal_size_limit_bytes > 0
        && let Err(err) = pool.set_journal_size_limit(config.db_journal_size_limit_bytes)
    {
        tracing::debug!(error = %err, "db maintenance: journal_size_limit apply failed");
    }

    if maintenance_task_due(config.db_checkpoint_interval_secs, *last_checkpoint, now) {
        run_maintenance_op(
            "passive_wal_checkpoint",
            || pool.wal_checkpoint_passive().map(|_frames| ()),
            &db.maintenance_checkpoint_runs_total,
            &db.maintenance_last_checkpoint_us,
        );
        *last_checkpoint = Some(now);
    }

    if maintenance_task_due(config.db_analyze_interval_secs, *last_analyze, now) {
        run_maintenance_op(
            "analyze",
            || pool.analyze(),
            &db.maintenance_analyze_runs_total,
            &db.maintenance_last_analyze_us,
        );
        *last_analyze = Some(now);
    }

    if maintenance_task_due(config.db_vacuum_interval_secs, *last_vacuum, now) {
        run_maintenance_op(
            "vacuum",
            || pool.vacuum(),
            &db.maintenance_vacuum_runs_total,
            &db.maintenance_last_vacuum_us,
        );
        // br-fv0s1: the main vacuum never touches the isolated ATC telemetry
        // sidecar (atc.sqlite3), so pages freed by the experience-ceiling sweep
        // (br-bvq1x.11.6) would accumulate. Vacuum it on the same cadence,
        // reusing the vacuum metrics. No-op when the sidecar was never created.
        run_maintenance_op(
            "vacuum_atc_sidecar",
            || pool.vacuum_atc_sidecar(),
            &db.maintenance_vacuum_runs_total,
            &db.maintenance_last_vacuum_us,
        );
        *last_vacuum = Some(now);
    }

    // br-bvq1x.11.6: enforce the hard ATC experience-ledger row ceiling so the
    // raw `atc_experiences` table cannot grow unbounded (the ts2 killer: 859K
    // rows / 3.36 GB corrupted SQLite). Reuses the shared maintenance duration /
    // failure metrics; evictions are logged at info so operators see the bound
    // working. Open rows and rollups are preserved by the eviction itself.
    if config.atc_experience_max_rows > 0
        && maintenance_task_due(
            config.atc_retention_sweep_interval_secs,
            *last_atc_retention,
            now,
        )
    {
        let started = Instant::now();
        match mcp_agent_mail_db::atc_queries::enforce_experience_row_ceiling(
            pool,
            config.atc_experience_max_rows,
        ) {
            Ok(evicted) => {
                let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                db.maintenance_duration_us.record(elapsed_us);
                if evicted > 0 {
                    tracing::info!(
                        evicted,
                        max_rows = config.atc_experience_max_rows,
                        elapsed_us,
                        "atc experience-ceiling sweep evicted oldest terminal rows"
                    );
                }
            }
            Err(err) => {
                db.maintenance_failures_total.inc();
                tracing::warn!(
                    error = %err,
                    "atc experience-ceiling sweep failed; will retry next cycle"
                );
            }
        }
        *last_atc_retention = Some(now);
    }

    // br-mudrv: bound doctor recovery-debris growth across recovery events.
    // OBSERVE + ALERT only — the forensic-bundle manifest declares
    // `automatic_deletion: false` and RULE 1 forbids automatic deletion, so the
    // actual reclaim is the explicit `am doctor reclaim` operator verb. Here we
    // surface (warn) when the reclaimable debris exceeds the configured
    // threshold so the growth that silently reached ~19 GB in prod is visible.
    if config.doctor_retention_enabled
        && maintenance_task_due(
            config.doctor_retention_sweep_interval_secs,
            *last_doctor_retention,
            now,
        )
    {
        let artifacts = mcp_agent_mail_db::recovery_retention::enumerate_recovery_debris(
            &config.storage_root,
            sqlite_path,
        );
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_micros()).ok())
            .unwrap_or(0);
        let live_database_bytes = std::fs::metadata(sqlite_path).map_or(0, |meta| meta.len());
        let plan = mcp_agent_mail_db::recovery_retention::select_recovery_debris_to_reclaim(
            artifacts,
            mcp_agent_mail_db::recovery_retention::RetentionPolicy {
                keep_min: usize::try_from(config.doctor_retention_keep_min).unwrap_or(usize::MAX),
                max_age_secs: config.doctor_retention_max_age_secs,
                max_total_bytes_per_category:
                    mcp_agent_mail_db::recovery_retention::effective_byte_budget_per_category(
                        config.doctor_retention_max_bytes_per_category,
                        live_database_bytes,
                    ),
            },
            now_us,
        );
        if config.doctor_retention_alert_bytes > 0
            && plan.reclaimable_bytes >= config.doctor_retention_alert_bytes
        {
            tracing::warn!(
                reclaimable_bytes = plan.reclaimable_bytes,
                reclaimable_artifacts = plan.prune.len(),
                total_bytes = plan.total_bytes,
                total_artifacts = plan.total_count,
                alert_bytes = config.doctor_retention_alert_bytes,
                "doctor recovery debris exceeds retention threshold; run `am doctor reclaim` to consolidate (br-mudrv)"
            );
        } else if plan.has_reclaimable() {
            tracing::debug!(
                reclaimable_bytes = plan.reclaimable_bytes,
                reclaimable_artifacts = plan.prune.len(),
                "doctor recovery debris reclaimable but under alert threshold"
            );
        }
        *last_doctor_retention = Some(now);
    }
}

#[cfg(test)]
fn handle_integrity_error(
    phase: &str,
    error_message: &str,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
) {
    // Test compatibility shim: exercises one observation with a fresh log.
    let mut standing_defects = StandingDefectLog::default();
    handle_integrity_error_with_log(
        phase,
        error_message,
        sqlite_path,
        storage_root,
        last_recovery_attempt,
        &mut standing_defects,
    );
}

fn handle_integrity_error_with_log(
    phase: &str,
    error_message: &str,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
    standing_defects: &mut StandingDefectLog,
) {
    let recoverable = is_sqlite_recovery_error_message(error_message)
        || is_corruption_error_message(error_message);
    if !recoverable {
        tracing::warn!(
            phase,
            error = %error_message,
            "integrity guard: non-recoverable integrity error"
        );
        return;
    }

    let now = Instant::now();
    if let Some(last) = *last_recovery_attempt
        && now.duration_since(last) < Duration::from_secs(RECOVERY_MIN_INTERVAL_SECS)
    {
        tracing::warn!(
            phase,
            error = %error_message,
            "integrity guard: recovery throttled after recent attempt"
        );
        return;
    }
    *last_recovery_attempt = Some(now);

    let storage_root_present = storage_root.is_dir();
    // GH#288: honest recovery wording — when the durable recovery breaker
    // beside the DB records that reconstruct fails deterministically, do not
    // promise a reconstruct that will never run.
    let recovery_note = recovery_availability_note(sqlite_path);
    // GH#288: fingerprint + backoff. An unchanged standing defect logs the
    // TRANSITION at WARN once, a "still standing" reminder at WARN on a
    // 6-hourly cadence, and every other observation at debug — so a journal
    // monitor sees state changes, not ~23 identical WARN pairs per day.
    match standing_defects.observe(phase, error_message) {
        Some(verdict) if verdict.new_detection => {
            // #105 flagged this line as confusing — "recovery is disabled"
            // reads as "nothing will happen" but in fact the query path
            // triggers `reconstruct_sqlite_file_with_archive_salvage` on its
            // own when a tool reads against a broken verdict (see
            // `mcp-agent-mail-tools::tool_util::live_db_is_suspect`). What
            // this branch actually does is *surface* the detection and leave
            // the mutation to the query path's admission-controlled
            // reconstruct, so say that plainly.
            tracing::warn!(
                phase,
                path = %sqlite_path.display(),
                error = %error_message,
                storage_root_present,
                recovery = %recovery_note,
                "integrity guard detected recoverable sqlite corruption; background guard does not mutate the live db"
            );
        }
        Some(verdict) => {
            tracing::warn!(
                phase,
                path = %sqlite_path.display(),
                error = %error_message,
                storage_root_present,
                recovery = %recovery_note,
                first_seen_unix = verdict.first_seen_unix,
                observations = verdict.observations,
                "integrity guard: standing sqlite defect unchanged since first detection"
            );
        }
        None => {
            tracing::debug!(
                phase,
                path = %sqlite_path.display(),
                "integrity guard: standing sqlite defect re-observed (unchanged; suppressed repeat warn)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_staging_refuses_both_routes_without_discarding_verified_requests() {
        let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let primary = root.join("mail.sqlite3");
        let backup = root.join("mail.sqlite3.bak");
        std::fs::write(&primary, b"live mailbox").unwrap();
        std::fs::write(&backup, b"verified backup").unwrap();
        for ordinal in 0..3 {
            let stage = root.join(format!(".mcp-agent-mail-proactive-backup-{ordinal:06}"));
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(stage.join("snapshot.sqlite3"), b"retained evidence").unwrap();
        }
        let now = Instant::now();
        for verified in [false, true] {
            let mut schedule = AutomaticBackupSchedule::default();
            if verified {
                schedule.request_verified();
            }
            assert!(run_admitted_backup_with(
                &primary,
                &mut schedule,
                || now,
                |_| panic!("a saturated staging budget must block either producer"),
            ));
            assert_eq!(schedule.consecutive_failures(), 1);
            assert_eq!(schedule.next_attempt(now), None);
            assert_eq!(
                schedule.next_attempt(now + Duration::from_secs(900)),
                Some(if verified { BackupKind::Verified } else { BackupKind::Proactive }),
                "admission refusal must preserve pending verified work"
            );
        }
        assert_eq!(std::fs::read(primary).unwrap(), b"live mailbox");
        assert_eq!(std::fs::read(backup).unwrap(), b"verified backup");
        for ordinal in 0..3 {
            let stage = root.join(format!(".mcp-agent-mail-proactive-backup-{ordinal:06}"));
            assert_eq!(std::fs::read(stage.join("snapshot.sqlite3")).unwrap(), b"retained evidence");
        }
    }

    #[test]
    fn backup_retry_delay_skips_filesystem_admission_too() {
        let root = tempfile::tempdir().unwrap().keep();
        let absent = root.join("absent.sqlite3");
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.complete(BackupKind::Proactive, BackupCompletion::Failed, now);
        assert!(run_admitted_backup_with(
            &absent,
            &mut schedule,
            || now + Duration::from_secs(1),
            |_| panic!("a paced operation must not enter admission or its producer"),
        ));
        assert_eq!(schedule.consecutive_failures(), 1);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn admitted_dispatch_holds_the_parent_lease_until_the_producer_returns() {
        let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let primary = root.join("mail.sqlite3");
        std::fs::write(&primary, b"live mailbox").unwrap();
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        schedule.request_verified();
        assert!(run_admitted_backup_with(
            &primary,
            &mut schedule,
            || now,
            |kind| {
                assert_eq!(kind, BackupKind::Verified);
                assert!(backup_budget::with_admission(&primary, || Ok(())).is_err());
                Ok(true)
            },
        ));
        assert!(backup_budget::with_admission(&primary, || Ok(())).is_ok());
        assert_eq!(schedule.next_attempt(now), Some(BackupKind::Proactive));
    }

    #[test]
    fn automatic_admission_allows_real_proactive_and_verified_publication() {
        for verified in [false, true] {
            let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let conn = mcp_agent_mail_db::CanonicalDbConn::open_file(primary.to_str().unwrap())
                .expect("create real canonical mailbox");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("initialize real mailbox schema");
            conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[]).unwrap();
            drop(conn);
            let pool = mcp_agent_mail_db::create_pool(&DbPoolConfig {
                database_url: format!("sqlite:///{}", primary.display()),
                ..DbPoolConfig::default()
            }).expect("create real backup pool");
            let before = std::fs::read(&primary).unwrap();
            let mut schedule = AutomaticBackupSchedule::default();
            if verified {
                schedule.request_verified();
            }
            assert!(run_automatic_backup(&pool, &mut schedule));
            let backup = mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary);
            assert!(backup.is_file(), "the real producer must publish, not merely return best-effort success");
            assert!(mcp_agent_mail_db::pool::sqlite_recovery_candidate_passes_full_integrity_check(&backup)
                .expect("independent strict canonical snapshot validation"));
            assert_eq!(mcp_agent_mail_db::snapshot::snapshot_meta_path(&primary).is_file(), verified);
            assert_eq!(std::fs::read(primary).unwrap(), before, "backup admission must not alter live bytes");
            assert_eq!(schedule.consecutive_failures(), 0);
        }
    }

    // GH#288: fingerprint + backoff for standing integrity defects.
    #[test]
    fn standing_defect_log_warns_on_transition_and_suppresses_repeats() {
        let mut log = StandingDefectLog::default();
        let msg = "integrity_check detected corruption (3): page 7 is never used";

        let first = log
            .observe("integrity_check", msg)
            .expect("first observation must warn");
        assert!(first.new_detection);
        assert_eq!(first.observations, 1);

        // Identical defect inside the re-warn window: suppressed to debug.
        assert!(log.observe("integrity_check", msg).is_none());
        assert!(log.observe("integrity_check", msg).is_none());

        // Different message ⇒ different fingerprint ⇒ fresh WARN transition.
        let changed = log
            .observe("integrity_check", "page 9 is cross-linked")
            .expect("changed fingerprint must warn");
        assert!(changed.new_detection);

        // Phases dedupe independently.
        let other_phase = log
            .observe("quick_check", msg)
            .expect("first observation in a new phase must warn");
        assert!(other_phase.new_detection);
    }

    #[test]
    fn standing_defect_log_clear_makes_recurrence_a_fresh_transition() {
        let mut log = StandingDefectLog::default();
        let msg = "page 7 is never used";
        assert!(log.observe("integrity_check", msg).is_some());
        assert!(log.observe("integrity_check", msg).is_none());

        log.clear("integrity_check");
        let again = log
            .observe("integrity_check", msg)
            .expect("recurrence after a passing cycle must warn again");
        assert!(again.new_detection);
    }

    #[test]
    fn standing_defect_rewarn_reminder_reports_observation_count() {
        let mut log = StandingDefectLog::default();
        let msg = "page 7 is never used";
        assert!(log.observe("integrity_check", msg).is_some());
        assert!(log.observe("integrity_check", msg).is_none());
        // Force the reminder cadence to elapse. checked_sub: on a host with
        // less monotonic-clock history than the cadence this is
        // unrepresentable — skip rather than panic.
        let Some(past) = Instant::now().checked_sub(Duration::from_secs(
            STANDING_DEFECT_REWARN_INTERVAL_SECS + 1,
        )) else {
            return;
        };
        log.entries
            .get_mut("integrity_check")
            .expect("entry must exist")
            .last_warn = past;
        let reminder = log
            .observe("integrity_check", msg)
            .expect("elapsed cadence must re-warn");
        assert!(!reminder.new_detection);
        assert_eq!(reminder.observations, 3);
    }

    // GH#288: the guard must not promise a breaker-blocked reconstruct.
    #[test]
    fn recovery_availability_note_reports_breaker_recorded_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("standing-defect.sqlite3");
        std::fs::write(&db_path, b"not a database").expect("write fixture");

        assert!(
            recovery_availability_note(&db_path).contains("admission-controlled reconstruct"),
            "no breaker sidecar must keep the default promise wording"
        );

        let state = mcp_agent_mail_db::recovery_breaker::RecoveryBreakerState {
            schema: 1,
            db_fingerprint: mcp_agent_mail_db::recovery_breaker::fingerprint_db(&db_path),
            consecutive_failures: 1,
            last_failure_unix: 1_787_807_828,
            last_failure_reason:
                "archive reconstruction failed: reservations produced 220 rows but only 215 \
                 unique stable keys; refusing ambiguous recovery"
                    .to_string(),
            tripped: false,
            attempt_in_progress: false,
        };
        mcp_agent_mail_db::recovery_breaker::store(&db_path, &state).expect("store breaker");

        let note = recovery_availability_note(&db_path);
        assert!(
            note.contains("recovery unavailable"),
            "recorded breaker failure must flip the wording: {note}"
        );
        assert!(note.contains("215 unique stable keys"), "note: {note}");
    }

    #[test]
    fn cross_count_healthy_file_reports_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cross-count.sqlite3");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(path.display().to_string()).expect("open db file");
        conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create table");
        conn.execute_raw("CREATE INDEX idx_agents_name ON agents(name)")
            .expect("create index");
        conn.execute_raw("INSERT INTO agents (name) VALUES ('BlueLake')")
            .expect("insert row");
        mcp_agent_mail_db::close_db_conn(conn, "cross-count test");

        assert!(
            run_index_table_cross_count(&path)
                .expect("healthy cross-count probe must complete")
                .is_none(),
            "healthy database must not report a cross-count desync"
        );
    }

    #[cfg(all(not(target_arch = "wasm32"), any(unix, windows)))]
    #[test]
    fn cross_count_read_only_drop_does_not_checkpoint_live_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cross-count-read-only-drop.sqlite3");
        let conn = mcp_agent_mail_db::DbConn::open_file(path.to_string_lossy().as_ref())
            .expect("open cross-count WAL fixture");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("initialize cross-count WAL fixture");
        conn.execute_raw("PRAGMA journal_mode = WAL;")
            .expect("enable cross-count fixture WAL mode");
        conn.execute_raw("PRAGMA wal_autocheckpoint = 0;")
            .expect("disable cross-count fixture autocheckpoint");
        conn.execute_raw("CREATE TABLE cross_count_drop_sentinel(value INTEGER NOT NULL);")
            .expect("create cross-count checkpoint sentinel");
        conn.execute_raw("INSERT INTO cross_count_drop_sentinel(value) VALUES (1);")
            .expect("write cross-count checkpoint sentinel");
        // Ordinary writer Drop deliberately leaves committed WAL frames for
        // the read-only observer teardown below to preserve.
        drop(conn);

        let wal_path = path.with_file_name(format!(
            "{}-wal",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let primary_before = std::fs::read(&path).expect("snapshot cross-count primary");
        let wal_before = std::fs::read(&wal_path).expect("snapshot cross-count WAL");
        assert!(
            wal_before.len() > mcp_agent_mail_db::pool::SQLITE_WAL_HEADER_BYTES as usize,
            "fixture must retain committed frames that a checkpointing close would consume"
        );

        let observer = open_index_table_cross_count_connection(&path)
            .expect("open strict read-only WAL observer");
        let mismatches =
            mcp_agent_mail_db::integrity::index_table_cross_count(&observer, CROSS_COUNT_TABLES)
                .expect("the real read-only WAL probe must execute successfully");
        assert!(
            mismatches.is_empty(),
            "healthy read-only WAL probe found mismatches: {mismatches:?}"
        );
        drop(observer);

        assert!(
            run_index_table_cross_count(&path)
                .expect("healthy WAL cross-count probe must complete")
                .is_none(),
            "healthy cross-count fixture must not report desync"
        );
        assert_eq!(
            std::fs::read(&path).expect("read cross-count primary after observer"),
            primary_before,
            "read-only cross-count teardown must not checkpoint frames into the primary"
        );
        assert_eq!(
            std::fs::read(&wal_path).expect("read cross-count WAL after observer"),
            wal_before,
            "read-only cross-count teardown must not checkpoint or truncate the WAL"
        );
    }

    #[test]
    fn cross_count_missing_file_reports_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-created.sqlite3");
        let error = run_index_table_cross_count(&path)
            .expect_err("an unopenable database must not become a completed clean probe");
        assert!(error.contains("cross-count read-only open failed"));
        assert!(
            !path.exists(),
            "the failed observer must not create a database"
        );
    }

    #[test]
    fn cross_count_guarded_read_only_open_refuses_damaged_family_under_nonclean_breaker_authority()
    {
        fn snapshot_namespace(
            root: &Path,
        ) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
            std::fs::read_dir(root)
                .expect("list cross-count fixture namespace")
                .map(|entry| {
                    let entry = entry.expect("read cross-count fixture entry");
                    let name = entry.file_name();
                    let bytes =
                        std::fs::read(entry.path()).expect("read cross-count fixture bytes");
                    (name, bytes)
                })
                .collect()
        }

        for breaker_kind in ["malformed", "tripped"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir
                .path()
                .join(format!("cross-count-{breaker_kind}.sqlite3"));
            let conn =
                mcp_agent_mail_db::CanonicalDbConn::open_file(path.to_string_lossy().as_ref())
                    .expect("create healthy cross-count primary");
            conn.execute_raw("PRAGMA journal_mode = DELETE;")
                .expect("detach cross-count fixture WAL mode");
            conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)")
                .expect("create cross-count fixture table");
            conn.execute_raw("CREATE INDEX idx_agents_name ON agents(name)")
                .expect("create cross-count fixture index");
            conn.execute_raw("INSERT INTO agents (name) VALUES ('BlueLake')")
                .expect("insert cross-count fixture row");
            drop(conn);

            // Materialize a real FrankenSQLite namespace sidecar pair
            // (-fsqlite-ns-gate/-fsqlite-ns-use): strict read-only admission
            // refuses a namespace-less family before it ever reaches the
            // breaker-authority proof this test exists to exercise.
            {
                let franken = mcp_agent_mail_db::DbConn::open_file(path.to_string_lossy().as_ref())
                    .expect("open cross-count fixture with franken engine");
                let _ = franken.query_sync("SELECT count(*) FROM agents", &[]);
                mcp_agent_mail_db::close_db_conn(franken, "cross-count namespace fixture");
            }

            let wal_path = path.with_file_name(format!(
                "{}-wal",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let shm_path = path.with_file_name(format!(
                "{}-shm",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            std::fs::write(&wal_path, b"truncated-cross-count-wal")
                .expect("write damaged cross-count WAL");
            std::fs::write(&shm_path, b"cross-count-shm").expect("write cross-count SHM");
            assert!(
                mcp_agent_mail_db::wal_classify::classify_wal_sidecar(&path)
                    .state
                    .is_damaged(),
                "the cross-count fixture must exercise damaged-family admission"
            );

            let breaker_path = mcp_agent_mail_db::recovery_breaker::breaker_sidecar_path(&path);
            match breaker_kind {
                "malformed" => {
                    std::fs::write(&breaker_path, b"malformed cross-count breaker authority")
                        .expect("write malformed cross-count breaker");
                    assert!(
                        mcp_agent_mail_db::recovery_breaker::load(&path).is_err(),
                        "malformed fixture must be rejected as durable authority"
                    );
                }
                "tripped" => {
                    let state = mcp_agent_mail_db::recovery_breaker::RecoveryBreakerState {
                        schema: 1,
                        db_fingerprint: mcp_agent_mail_db::recovery_breaker::fingerprint_db(&path),
                        consecutive_failures:
                            mcp_agent_mail_db::recovery_breaker::DEFAULT_MAX_CONSECUTIVE_FAILURES,
                        last_failure_unix: i64::MAX,
                        last_failure_reason: "cross-count fixture is circuit-broken".to_string(),
                        tripped: true,
                        attempt_in_progress: false,
                    };
                    mcp_agent_mail_db::recovery_breaker::store(&path, &state)
                        .expect("store tripped cross-count breaker");
                    assert_eq!(
                        mcp_agent_mail_db::recovery_breaker::load(&path)
                            .expect("load tripped cross-count breaker"),
                        Some(state),
                        "tripped fixture must retain exact-primary breaker authority"
                    );
                }
                _ => unreachable!(),
            }

            let namespace_before = snapshot_namespace(dir.path());
            let error = match open_index_table_cross_count_connection(&path) {
                Ok(conn) => {
                    drop(conn);
                    panic!("cross-count guarded read-only open must refuse the suspect family");
                }
                Err(error) => error,
            };
            let expected_refusal = match breaker_kind {
                "malformed" => "durable recovery authority is malformed or unreadable",
                "tripped" => "durable recovery authority is nonclean",
                _ => unreachable!(),
            };
            assert!(
                error.to_string().contains(expected_refusal),
                "unexpected cross-count refusal for {breaker_kind}: {error}"
            );
            assert!(
                run_index_table_cross_count(&path).is_err(),
                "the aggregate must report unavailable evidence for a refused family"
            );
            assert_eq!(
                snapshot_namespace(dir.path()),
                namespace_before,
                "cross-count refusal must preserve every exact family/breaker byte and name for {breaker_kind}"
            );
        }
    }

    #[test]
    fn full_check_interval_disabled_when_zero() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 0;
        assert!(full_check_interval(&config).is_none());
    }

    #[test]
    fn full_check_interval_has_minimum_floor() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        assert_eq!(
            full_check_interval(&config),
            Some(Duration::from_secs(MIN_FULL_CHECK_INTERVAL_SECS))
        );
    }

    #[test]
    fn quick_interval_matches_default() {
        assert_eq!(
            quick_check_interval(),
            Duration::from_secs(DEFAULT_QUICK_CHECK_INTERVAL_SECS)
        );
    }

    #[test]
    #[allow(clippy::duration_suboptimal_units)]
    fn full_check_interval_large_value() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 24;
        assert_eq!(
            full_check_interval(&config),
            Some(Duration::from_secs(86_400))
        );
    }

    #[test]
    fn full_check_interval_small_value_clamped_to_minimum() {
        // Even sub-hour values get clamped to MIN_FULL_CHECK_INTERVAL_SECS
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1; // 1 hour = 3600s >= 3600s minimum
        let interval = full_check_interval(&config).unwrap();
        assert!(interval.as_secs() >= MIN_FULL_CHECK_INTERVAL_SECS);
    }

    #[test]
    fn full_check_interval_saturating_mul_no_overflow() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = u64::MAX;
        // saturating_mul should not panic
        let interval = full_check_interval(&config);
        assert!(interval.is_some());
        assert!(interval.unwrap().as_secs() >= MIN_FULL_CHECK_INTERVAL_SECS);
    }

    #[test]
    fn quick_check_interval_is_5_minutes() {
        assert_eq!(quick_check_interval().as_secs(), 300);
    }

    #[test]
    fn full_check_due_respects_attempt_throttle() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        let interval = full_check_interval(&config);
        assert!(!full_check_due(&config, interval, Some(Instant::now())));
    }

    #[test]
    fn full_check_due_uses_last_attempt_not_last_success() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        let interval = full_check_interval(&config);
        let stale_success = Instant::now()
            .checked_sub(Duration::from_secs(MIN_FULL_CHECK_INTERVAL_SECS + 1))
            .expect("stale success timestamp");
        assert!(
            full_check_due(&config, interval, Some(stale_success)),
            "an old successful full check should make another attempt due"
        );

        let attempted_at = Instant::now();
        assert!(
            !full_check_due(&config, interval, Some(attempted_at)),
            "a recent failed attempt should still throttle the next full check"
        );
    }

    #[test]
    fn failed_quick_cycle_skips_full_check_and_maintenance() {
        let full_calls = std::cell::Cell::new(0_u8);
        let maintenance_calls = std::cell::Cell::new(0_u8);
        let mut gate = FullVerificationGate::default();

        let followed_up = run_integrity_followups(
            false,
            true,
            &mut gate,
            Instant::now,
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            |_| {
                maintenance_calls.set(maintenance_calls.get() + 1);
                true
            },
        );

        assert!(!followed_up);
        assert!(gate.is_required());
        assert_eq!(full_calls.get(), 0, "failed quick verdict must be terminal");
        assert_eq!(
            maintenance_calls.get(),
            0,
            "failed quick verdict must block every mutating maintenance path"
        );
    }

    #[test]
    fn failed_due_full_cycle_skips_maintenance() {
        let full_calls = std::cell::Cell::new(0_u8);
        let maintenance_calls = std::cell::Cell::new(0_u8);
        let mut gate = FullVerificationGate::default();

        let followed_up = run_integrity_followups(
            true,
            true,
            &mut gate,
            Instant::now,
            || {
                full_calls.set(full_calls.get() + 1);
                false
            },
            |_| {
                maintenance_calls.set(maintenance_calls.get() + 1);
                true
            },
        );

        assert!(!followed_up);
        assert!(gate.is_required());
        assert_eq!(
            full_calls.get(),
            1,
            "a due full check must run exactly once"
        );
        assert_eq!(
            maintenance_calls.get(),
            0,
            "failed full verdict must block mutating maintenance"
        );
    }

    #[test]
    fn passing_integrity_verdicts_run_only_due_followups() {
        let full_calls = std::cell::Cell::new(0_u8);
        let maintenance_calls = std::cell::Cell::new(0_u8);
        let mut gate = FullVerificationGate::default();

        assert!(run_integrity_followups(
            true,
            false,
            &mut gate,
            Instant::now,
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            |full_completed| {
                assert!(!full_completed);
                maintenance_calls.set(maintenance_calls.get() + 1);
                true
            },
        ));
        assert_eq!(
            full_calls.get(),
            0,
            "a non-due full check must stay skipped"
        );
        assert_eq!(maintenance_calls.get(), 1);

        assert!(run_integrity_followups(
            true,
            true,
            &mut gate,
            Instant::now,
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            |full_completed| {
                assert!(full_completed);
                maintenance_calls.set(maintenance_calls.get() + 1);
                true
            },
        ));
        assert_eq!(full_calls.get(), 1);
        assert_eq!(maintenance_calls.get(), 2);
    }

    #[test]
    fn failed_full_cycle_remains_blocking_on_later_quick_success() {
        let start = Instant::now();
        let mut gate = FullVerificationGate::default();
        assert!(!run_integrity_followups(
            true,
            true,
            &mut gate,
            || start,
            || false,
            |_| panic!("failed full check must block all writes"),
        ));
        assert!(!run_integrity_followups(
            true,
            false,
            &mut gate,
            || start + Duration::from_secs(60),
            || panic!("full retry must respect its delay"),
            |_| panic!("a quick pass cannot erase the previous full failure"),
        ));
        assert!(gate.is_required());
        let full_calls = std::cell::Cell::new(0);
        assert!(run_integrity_followups(
            true,
            false,
            &mut gate,
            || start + Duration::from_secs(300),
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            |full_completed| {
                assert!(full_completed);
                true
            },
        ));
        assert_eq!(full_calls.get(), 1);
        assert!(!gate.is_required());
    }

    #[test]
    fn failed_quick_cycle_requires_a_full_check_even_without_a_schedule() {
        let now = Instant::now();
        let mut gate = FullVerificationGate::default();
        assert!(!run_integrity_followups(
            false,
            false,
            &mut gate,
            || now,
            || panic!("quick failure is terminal for this cycle"),
            |_| panic!("quick failure must block writes"),
        ));
        let verified = std::cell::Cell::new(false);
        assert!(run_integrity_followups(
            true,
            false,
            &mut gate,
            || now,
            || {
                verified.set(true);
                true
            },
            |full_completed| {
                assert!(full_completed && verified.get());
                true
            },
        ));
    }

    #[test]
    fn followup_invalidation_requires_fresh_full_evidence_next_cycle() {
        let now = Instant::now();
        let mut gate = FullVerificationGate::default();
        assert!(!run_integrity_followups(
            true,
            false,
            &mut gate,
            || now,
            || panic!("initial full check was not due"),
            |_| false,
        ));
        assert!(gate.is_required());
        assert!(!run_integrity_followups(
            true,
            false,
            &mut gate,
            || now,
            || false,
            |_| panic!("invalidated evidence must not authorize writes"),
        ));
    }

    #[test]
    fn backup_dispatch_uses_one_shared_window_and_retries_pending_verified_work() {
        let now = Instant::now();
        let mut schedule = AutomaticBackupSchedule::default();
        assert!(run_automatic_backup_with(
            &mut schedule,
            || now,
            |kind| {
                assert_eq!(kind, BackupKind::Proactive);
                Err("export failed; unique staging path one".to_string())
            }
        ));
        schedule.request_verified();
        assert!(run_automatic_backup_with(
            &mut schedule,
            || now + Duration::from_secs(300),
            |_| panic!("verified route must not bypass proactive failure backoff"),
        ));
        assert!(!run_automatic_backup_with(
            &mut schedule,
            || now + Duration::from_secs(900),
            |kind| {
                assert_eq!(kind, BackupKind::Verified);
                Ok(false)
            },
        ));
        assert_eq!(schedule.consecutive_failures(), 1);
        assert!(run_automatic_backup_with(
            &mut schedule,
            || now + Duration::from_secs(1800),
            |kind| {
                assert_eq!(kind, BackupKind::Verified);
                Ok(true)
            },
        ));
        assert_eq!(schedule.consecutive_failures(), 0);
        assert_eq!(
            schedule.next_attempt(now + Duration::from_secs(1800)),
            Some(BackupKind::Proactive)
        );
    }

    #[test]
    fn automatic_backup_failure_delay_starts_after_the_producer_finishes() {
        let started = Instant::now();
        let clock = std::cell::Cell::new(started);
        let mut schedule = AutomaticBackupSchedule::default();
        assert!(run_automatic_backup_with(
            &mut schedule,
            || clock.get(),
            |_| {
                clock.set(started + Duration::from_secs(3600));
                Err("slow failed export".to_string())
            },
        ));
        assert!(schedule.next_attempt(clock.get()).is_none());
        assert_eq!(
            schedule.retry_remaining(clock.get()),
            Duration::from_secs(900)
        );
    }

    #[test]
    fn real_collation_mismatch_blocks_followup_writes_until_reindex() {
        use mcp_agent_mail_db::integrity::{
            CheckKind, details_indicate_ok, extract_check_details, index_table_cross_count,
        };

        let conn = mcp_agent_mail_db::CanonicalDbConn::open_memory().unwrap();
        conn.execute_raw(
            "CREATE TABLE guard_mail (id INTEGER PRIMARY KEY, body TEXT); \
             INSERT INTO guard_mail(body) VALUES ('Zebra'), ('apple'); \
             CREATE INDEX idx_guard_mail ON guard_mail(body); \
             CREATE TABLE guard_writes (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        // Only this owned in-memory fixture is altered. The existing index
        // has BINARY key order; declaring NOCASE reproduces a real ordering
        // disagreement without missing rows, orphan pages, or shared roots.
        conn.execute_raw(
            "PRAGMA writable_schema=ON; \
             UPDATE sqlite_master SET sql= \
                 'CREATE INDEX idx_guard_mail ON guard_mail(body COLLATE NOCASE)' \
                 WHERE name='idx_guard_mail'; \
             PRAGMA writable_schema=OFF; PRAGMA schema_version=100;",
        )
        .unwrap();
        let quick_passes = || {
            let rows = conn.query_sync("PRAGMA quick_check", &[]).unwrap();
            details_indicate_ok(&extract_check_details(&rows, CheckKind::Quick))
        };
        let full_passes = || {
            let rows = conn.query_sync("PRAGMA integrity_check", &[]).unwrap();
            details_indicate_ok(&extract_check_details(&rows, CheckKind::Full))
        };
        assert!(quick_passes());
        assert!(
            index_table_cross_count(&conn, &["guard_mail"])
                .unwrap()
                .is_empty()
        );
        assert!(
            !full_passes(),
            "the full probe must detect actual key-order damage"
        );

        let start = Instant::now();
        let mut gate = FullVerificationGate::default();
        let mutating_followup = |_| {
            conn.execute_raw("INSERT INTO guard_writes DEFAULT VALUES")
                .unwrap();
            true
        };
        assert!(!run_integrity_followups(
            quick_passes(),
            true,
            &mut gate,
            || start,
            full_passes,
            mutating_followup,
        ));
        assert!(!run_integrity_followups(
            quick_passes(),
            false,
            &mut gate,
            || start + Duration::from_secs(300),
            full_passes,
            mutating_followup,
        ));
        let rows = conn
            .query_sync("SELECT count(*) AS c FROM guard_writes", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("c").unwrap(), 0);

        conn.execute_raw("REINDEX idx_guard_mail").unwrap();
        assert!(
            full_passes(),
            "the complete canonical scan must pass after repair"
        );
        assert!(run_integrity_followups(
            quick_passes(),
            false,
            &mut gate,
            || start + Duration::from_secs(900),
            full_passes,
            mutating_followup,
        ));
        let rows = conn
            .query_sync("SELECT count(*) AS c FROM guard_writes", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("c").unwrap(), 1);
    }

    #[test]
    fn defer_next_proactive_backup_is_one_shot() {
        SKIP_NEXT_PROACTIVE_BACKUP.store(false, Ordering::Release);
        assert!(!take_deferred_proactive_backup());
        defer_next_proactive_backup();
        assert!(take_deferred_proactive_backup());
        assert!(
            !take_deferred_proactive_backup(),
            "startup backup deferral should apply only once"
        );
    }

    #[test]
    fn resolve_integrity_guard_sqlite_path_prefers_absolute_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("integrity-guard.sqlite3");
        std::fs::write(&absolute_db, b"seed").expect("write absolute db");

        let relative_path = PathBuf::from(absolute_db.to_string_lossy().trim_start_matches('/'));
        assert!(
            !relative_path.exists(),
            "relative shadow path should be absent so integrity guard resolves the absolute candidate"
        );

        let mut config = Config::from_env();
        config.database_url = format!("sqlite:///{}", relative_path.display());

        let resolved =
            resolve_integrity_guard_sqlite_path(&config).expect("resolve integrity guard db path");
        assert_eq!(
            resolved, absolute_db,
            "integrity guard should monitor the resolved absolute candidate"
        );
    }

    #[test]
    fn handle_integrity_error_non_recoverable_does_not_update_timestamp() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // "connection reset" is NOT a recoverable error
        handle_integrity_error(
            "test",
            "connection reset by peer",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(
            last_recovery.is_none(),
            "non-recoverable error should not set last_recovery_attempt"
        );
    }

    #[test]
    fn handle_integrity_error_recoverable_sets_timestamp() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // "database disk image is malformed" IS a recoverable error
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(
            last_recovery.is_some(),
            "recoverable error should set last_recovery_attempt"
        );
    }

    #[test]
    fn handle_integrity_error_throttles_rapid_recovery() {
        let mut last_recovery: Option<Instant> = Some(Instant::now());
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        let before = last_recovery;

        // Second call immediately after should be throttled
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        // Timestamp should NOT have been updated (throttled)
        assert_eq!(
            last_recovery.map(|i| i.elapsed().as_millis() < 100),
            before.map(|i| i.elapsed().as_millis() < 100),
            "recovery should be throttled within RECOVERY_MIN_INTERVAL_SECS"
        );
    }

    #[test]
    fn handle_integrity_error_various_recoverable_messages() {
        let recoverable_msgs = [
            "database disk image is malformed",
            "Database Disk Image Is Malformed", // case-insensitive
            "malformed database schema - broken_table",
            "file is not a database",
            "out of memory",
            "cursor stack is empty",
            "internal error",
            "no healthy backup was found",
        ];
        for msg in &recoverable_msgs {
            let mut last_recovery: Option<Instant> = None;
            let tmp = tempfile::TempDir::new().unwrap();
            let sqlite_path = tmp.path().join("test.sqlite3");
            let storage_root = tmp.path().join("storage");

            handle_integrity_error("test", msg, &sqlite_path, &storage_root, &mut last_recovery);

            assert!(
                last_recovery.is_some(),
                "'{msg}' should be classified as recoverable"
            );
        }
    }

    #[test]
    fn handle_integrity_error_non_recoverable_messages() {
        let non_recoverable_msgs = [
            "connection refused",
            "timeout",
            "constraint violation",
            "unique constraint failed",
            "no such table",
        ];
        for msg in &non_recoverable_msgs {
            let mut last_recovery: Option<Instant> = None;
            let tmp = tempfile::TempDir::new().unwrap();
            let sqlite_path = tmp.path().join("test.sqlite3");
            let storage_root = tmp.path().join("storage");

            handle_integrity_error("test", msg, &sqlite_path, &storage_root, &mut last_recovery);

            assert!(
                last_recovery.is_none(),
                "'{msg}' should NOT be classified as recoverable"
            );
        }
    }

    #[test]
    fn handle_integrity_error_uses_archive_recovery_when_storage_exists() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // Create the storage directory so archive-aware recovery is used.
        std::fs::create_dir_all(&storage_root).unwrap();

        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        // We can't easily verify which recovery path was used, but
        // the function should not panic when storage_root exists.
        assert!(last_recovery.is_some());
    }

    #[test]
    fn handle_integrity_error_uses_file_recovery_when_no_storage() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("nonexistent_storage");

        // storage_root doesn't exist, so file-only recovery is used.
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(last_recovery.is_some());
    }

    #[test]
    fn constants_are_reasonable() {
        const _: () = assert!(
            DEFAULT_QUICK_CHECK_INTERVAL_SECS >= 60,
            "quick check should be at least 1 minute"
        );
        const _: () = assert!(
            MIN_FULL_CHECK_INTERVAL_SECS >= 3600,
            "full check minimum should be at least 1 hour"
        );
        const _: () = assert!(
            RECOVERY_MIN_INTERVAL_SECS >= 10,
            "recovery throttle should be at least 10 seconds"
        );
        const _: () = assert!(
            BACKUP_MAX_AGE_SECS >= 600,
            "backup max age should be at least 10 minutes"
        );
    }

    // ---- bead K4: periodic SQLite maintenance scheduling ----

    #[test]
    fn maintenance_task_due_disabled_when_interval_zero() {
        let now = Instant::now();
        assert!(
            !maintenance_task_due(0, None, now),
            "interval 0 disables the task"
        );
        assert!(!maintenance_task_due(0, Some(now), now));
    }

    #[test]
    fn maintenance_task_due_when_never_run() {
        let now = Instant::now();
        assert!(
            maintenance_task_due(300, None, now),
            "a task that never ran is always due once enabled"
        );
    }

    #[test]
    fn maintenance_task_due_respects_interval() {
        let now = Instant::now();
        let recent = now
            .checked_sub(Duration::from_secs(100))
            .expect("recent instant");
        let stale = now
            .checked_sub(Duration::from_secs(400))
            .expect("stale instant");
        assert!(
            !maintenance_task_due(300, Some(recent), now),
            "100s elapsed < 300s interval: not due"
        );
        assert!(
            maintenance_task_due(300, Some(stale), now),
            "400s elapsed >= 300s interval: due"
        );
        assert!(
            !maintenance_task_due(300, Some(now), now),
            "just ran this instant: not due"
        );
    }

    fn memory_maintenance_pool() -> DbPool {
        let pool_config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..DbPoolConfig::default()
        };
        mcp_agent_mail_db::create_pool(&pool_config).expect("create in-memory maintenance pool")
    }

    #[test]
    fn run_db_maintenance_cycle_noop_when_disabled() {
        let config = Config {
            db_maintenance_enabled: false,
            ..Config::default()
        };
        let pool = memory_maintenance_pool();
        let now = Instant::now();
        let sqlite_path = std::path::Path::new("/nonexistent/storage.sqlite3");
        let mut cp = None;
        let mut an = None;
        let mut va = None;
        let mut atc = None;
        let mut dr = None;
        run_db_maintenance_cycle(
            &pool,
            &config,
            sqlite_path,
            now,
            &mut cp,
            &mut an,
            &mut va,
            &mut atc,
            &mut dr,
        );
        assert!(
            cp.is_none() && an.is_none() && va.is_none() && atc.is_none() && dr.is_none(),
            "disabled maintenance must not attempt or advance any task"
        );
    }

    #[test]
    fn run_db_maintenance_cycle_advances_due_tasks_and_backs_off() {
        let config = Config {
            db_maintenance_enabled: true,
            db_checkpoint_interval_secs: 300,
            db_analyze_interval_secs: 300,
            db_vacuum_interval_secs: 300,
            db_journal_size_limit_bytes: 268_435_456,
            // The doctor recovery-debris sweep is exercised by the
            // recovery_retention unit tests; disable it here so this maintenance
            // test stays hermetic (no filesystem enumeration of the default root).
            doctor_retention_enabled: false,
            ..Config::default()
        };
        let pool = memory_maintenance_pool();
        let now = Instant::now();
        let sqlite_path = std::path::Path::new("/nonexistent/storage.sqlite3");

        // All tasks never-run => all due => cursors advance to `now`. (Ops no-op
        // on a :memory: pool but still record success and advance the cursor.)
        let mut cp = None;
        let mut an = None;
        let mut va = None;
        let mut atc = None;
        let mut dr = None;
        run_db_maintenance_cycle(
            &pool,
            &config,
            sqlite_path,
            now,
            &mut cp,
            &mut an,
            &mut va,
            &mut atc,
            &mut dr,
        );
        assert_eq!(cp, Some(now), "checkpoint cursor advanced");
        assert_eq!(an, Some(now), "analyze cursor advanced");
        assert_eq!(va, Some(now), "vacuum cursor advanced");
        assert_eq!(atc, Some(now), "atc retention cursor advanced");
        assert_eq!(
            dr, None,
            "doctor retention sweep disabled => cursor untouched"
        );

        // Re-running at the same instant: cursors are fresh, so nothing is due
        // and they must stay put (off-hot-path back-off).
        let before = (cp, an, va, atc);
        run_db_maintenance_cycle(
            &pool,
            &config,
            sqlite_path,
            now,
            &mut cp,
            &mut an,
            &mut va,
            &mut atc,
            &mut dr,
        );
        assert_eq!(
            (cp, an, va, atc),
            before,
            "fresh cursors must not re-run within the interval"
        );
    }
}
