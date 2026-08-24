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

/// Skip only the next proactive backup refresh while still performing the
/// integrity guard's quick health check.
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
            )
        };

        let full_due = full_check_due(config, full_every, last_full_attempt);
        run_integrity_followups(
            quick_cycle_passed,
            full_due,
            || {
                let attempted_at = Instant::now();
                let passed = run_full_cycle(
                    &pool,
                    sqlite_path,
                    &storage_root,
                    &mut last_recovery_attempt,
                );
                last_full_attempt = Some(attempted_at);
                passed
            },
            || {
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
            },
        );

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

/// Run the full-check and maintenance followups only after the integrity
/// verdicts that authorize them.
///
/// Keeping this sequencing in one small seam makes it impossible for a quick
/// breaker refusal to fall through into a second live-family open, checkpoint,
/// ANALYZE, or VACUUM. When a due full check fails, maintenance is likewise
/// skipped for this iteration.
fn run_integrity_followups<F, M>(
    quick_cycle_passed: bool,
    full_due: bool,
    run_full: F,
    run_maintenance: M,
) -> bool
where
    F: FnOnce() -> bool,
    M: FnOnce(),
{
    if !quick_cycle_passed {
        tracing::warn!(
            "integrity guard: skipping full check and database maintenance after failed quick cycle"
        );
        return false;
    }
    if full_due && !run_full() {
        tracing::warn!(
            "integrity guard: skipping database maintenance after failed full integrity cycle"
        );
        return false;
    }
    run_maintenance();
    true
}

fn run_quick_cycle(
    pool: &DbPool,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
) -> bool {
    match pool.run_periodic_integrity_check() {
        Ok(_) => {
            if take_deferred_proactive_backup() {
                tracing::debug!(
                    "integrity guard: deferred proactive backup during startup quick cycle"
                );
                return true;
            }
            if let Err(err) = pool.create_proactive_backup(Duration::from_secs(BACKUP_MAX_AGE_SECS))
            {
                tracing::warn!(error = %err, "integrity guard: proactive backup refresh failed");
            }
            // #219: a drift reconcile deferred at pool-bootstrap time
            // (cooldown or write activity) is otherwise lost until the next
            // promotion or restart — the per-path init gate latches. Retry
            // here on the quick cadence; every standalone pacing gate
            // (ownership, cooldown, write idleness) still applies inside,
            // so under sustained write load this stays a cheap no-op and
            // converges the first time the process goes write-quiet.
            match mcp_agent_mail_db::pool::retry_archive_drift_reconcile(sqlite_path, storage_root)
            {
                Ok(true) => tracing::info!(
                    "integrity guard: reconciled archive-ahead drift during quick cycle"
                ),
                Ok(false) => {}
                Err(err) => tracing::debug!(
                    error = %err,
                    "integrity guard: archive drift reconcile attempt failed; will retry next cycle"
                ),
            }
            true
        }
        Err(err) => {
            handle_integrity_error(
                "quick_check",
                &err.to_string(),
                sqlite_path,
                storage_root,
                last_recovery_attempt,
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
) -> bool {
    match pool.run_full_integrity_check() {
        Ok(_) => {
            tracing::info!("integrity guard: periodic full integrity check passed");
            // GH#214: PRAGMA checks can under-report the index/table desync
            // class (quick_check misses single-row loss; the GH#213 Linux
            // specimen sat green for a full run). Cross-count the hot tables
            // through their index btrees on the same full-check cadence.
            // Runs BEFORE the verified-snapshot capture so a desynced DB is
            // never recorded as last-known-healthy.
            if let Some(mismatch) = run_index_table_cross_count(sqlite_path) {
                handle_integrity_error(
                    "index_table_cross_count",
                    &mismatch,
                    sqlite_path,
                    storage_root,
                    last_recovery_attempt,
                );
                return false;
            }
            // Bead K2: a passing full check means the DB is verifiably clean —
            // capture a last-known-healthy verified snapshot (best-effort; the
            // call re-verifies and records metrics, and never fails the cycle).
            match pool.create_verified_snapshot() {
                Ok(Some(meta)) => tracing::debug!(
                    snapshot = %meta.snapshot_path,
                    "integrity guard: recorded verified snapshot"
                ),
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    error = %err,
                    "integrity guard: verified snapshot capture failed"
                ),
            }
            // Archive-ahead drift that first arises AFTER a clean bootstrap
            // (another process crashed between archive append and DB write,
            // manual/git archive edits, an external older-.bak restore) is
            // never in the pending-deferral set, so the quick cycle's cheap
            // gate short-circuits forever. The full cycle runs hourly and
            // already does O(DB-size) work, so run the drift predicate here
            // without the pending precondition; every standalone pacing gate
            // (ownership, cooldown, write idleness) still applies inside.
            match mcp_agent_mail_db::pool::reconcile_archive_drift_full_cycle(
                sqlite_path,
                storage_root,
            ) {
                Ok(true) => tracing::info!(
                    "integrity guard: reconciled post-bootstrap archive-ahead drift during full cycle"
                ),
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    error = %err,
                    "integrity guard: full-cycle archive drift reconcile failed"
                ),
            }
            true
        }
        Err(err) => {
            handle_integrity_error(
                "integrity_check",
                &err.to_string(),
                sqlite_path,
                storage_root,
                last_recovery_attempt,
            );
            false
        }
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
) -> std::io::Result<mcp_agent_mail_db::DbConn> {
    crate::open_read_only_sync_db_connection_with_busy_timeout(
        sqlite_path.to_string_lossy().as_ref(),
        crate::BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS,
        "integrity guard index/table cross-count",
    )
}

/// Run the GH#214 index-vs-table cross-count against a read-only connection.
///
/// Returns the first mismatch rendered as a corruption-classifiable message,
/// or `None` when every probed table agrees with its indexes (or the probe
/// itself could not run — probe failures are logged and are NOT corruption
/// evidence). Honest scope: this catches the desync class only; a mutually
/// consistent database missing acknowledged rows (the GH#213 Windows silent
/// class) is invisible to any server-side arithmetic.
fn run_index_table_cross_count(sqlite_path: &Path) -> Option<String> {
    let conn = match open_index_table_cross_count_connection(sqlite_path) {
        Ok(conn) => conn,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "integrity guard: cross-count read-only open failed; skipping this cycle"
            );
            return None;
        }
    };
    let result = mcp_agent_mail_db::integrity::index_table_cross_count(&conn, CROSS_COUNT_TABLES);
    // This is a true read-only observer. Its ordinary Drop path preserves the
    // live WAL/namespace family; the writable close helper may checkpoint it.
    drop(conn);
    match result {
        Ok(mismatches) => {
            if mismatches.is_empty() {
                return None;
            }
            for mismatch in &mismatches {
                tracing::error!(
                    table = %mismatch.table,
                    index = %mismatch.index,
                    table_rows = mismatch.table_rows,
                    index_rows = mismatch.index_rows,
                    "integrity guard: index/table cross-count desync (GH#214)"
                );
            }
            mismatches
                .first()
                .map(mcp_agent_mail_db::integrity::CrossCountMismatch::as_corruption_message)
        }
        Err(err) => {
            tracing::debug!(
                error = %err,
                "integrity guard: cross-count probe failed; skipping this cycle"
            );
            None
        }
    }
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

fn handle_integrity_error(
    phase: &str,
    error_message: &str,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
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
    // #105 flagged this line as confusing — "recovery is disabled" reads as
    // "nothing will happen" but in fact the query path triggers
    // `reconstruct_sqlite_file_with_archive_salvage` on its own when a
    // tool reads against a broken verdict (see
    // `mcp-agent-mail-tools::tool_util::live_db_is_suspect`). What this
    // branch actually does is *surface* the detection and leave the
    // mutation to the query path's admission-controlled reconstruct, so
    // say that plainly.
    tracing::warn!(
        phase,
        path = %sqlite_path.display(),
        error = %error_message,
        storage_root_present,
        "integrity guard detected recoverable sqlite corruption; background guard does not mutate the live db — the query-path recovery flow will trigger an admission-controlled reconstruct on the next corrupt-verdict read"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
            run_index_table_cross_count(&path).is_none(),
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

        assert!(
            run_index_table_cross_count(&path).is_none(),
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
    fn cross_count_missing_file_reports_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-created.sqlite3");
        assert!(
            run_index_table_cross_count(&path).is_none(),
            "an unopenable database is a probe failure, not corruption evidence"
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
            assert!(
                error
                    .to_string()
                    .contains("refusing live read-only SQLite engine open"),
                "unexpected cross-count refusal for {breaker_kind}: {error}"
            );
            assert!(
                run_index_table_cross_count(&path).is_none(),
                "the aggregate must degrade rather than inspect a refused family"
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

        let followed_up = run_integrity_followups(
            false,
            true,
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            || maintenance_calls.set(maintenance_calls.get() + 1),
        );

        assert!(!followed_up);
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

        let followed_up = run_integrity_followups(
            true,
            true,
            || {
                full_calls.set(full_calls.get() + 1);
                false
            },
            || maintenance_calls.set(maintenance_calls.get() + 1),
        );

        assert!(!followed_up);
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

        assert!(run_integrity_followups(
            true,
            false,
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            || maintenance_calls.set(maintenance_calls.get() + 1),
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
            || {
                full_calls.set(full_calls.get() + 1);
                true
            },
            || maintenance_calls.set(maintenance_calls.get() + 1),
        ));
        assert_eq!(full_calls.get(), 1);
        assert_eq!(maintenance_calls.get(), 2);
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
