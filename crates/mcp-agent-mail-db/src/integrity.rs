//! `SQLite` integrity checking for corruption detection and recovery.
//!
//! Provides three levels of checking:
//!
//! 1. **Quick check** (`PRAGMA quick_check`): Fast subset of integrity checks.
//!    Run on pool initialization when `INTEGRITY_CHECK_ON_STARTUP=true`.
//!
//! 2. **Incremental check** (`PRAGMA integrity_check(1)`): First-error-only check.
//!    Suitable for periodic connection-recycle validation.
//!
//! 3. **Full check** (`PRAGMA integrity_check`): Complete scan of the database.
//!    Run on a background schedule (default: every 24 hours).
//!
//! When corruption is detected, the system:
//! - Logs a CRITICAL error with the raw check output.
//! - Returns an `IntegrityCorruption` error so callers can set health to Red.
//! - Optionally attempts recovery via checkpoint + `VACUUM` + validated file copy.

use crate::DbConn;
use crate::error::{DbError, DbResult};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row, Value};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Result of an integrity check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityCheckResult {
    /// Whether the check passed (no corruption detected).
    pub ok: bool,
    /// Raw output lines from the PRAGMA.
    pub details: Vec<String>,
    /// Duration of the check in microseconds.
    pub duration_us: u64,
    /// Which kind of check was run.
    pub kind: CheckKind,
}

/// The kind of integrity check that was run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckKind {
    /// `PRAGMA quick_check` — fast subset.
    Quick,
    /// `PRAGMA integrity_check(1)` — first error only.
    Incremental,
    /// `PRAGMA integrity_check` — full scan.
    Full,
}

impl std::fmt::Display for CheckKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quick => write!(f, "quick_check"),
            Self::Incremental => write!(f, "integrity_check(1)"),
            Self::Full => write!(f, "integrity_check"),
        }
    }
}

/// Global state tracking the last integrity check result.
static LAST_CHECK: OnceLock<IntegrityCheckState> = OnceLock::new();

#[derive(Debug)]
struct IntegrityCheckState {
    /// Timestamp (microseconds since epoch) of the last successful check.
    last_ok_ts: AtomicI64,
    /// Timestamp (microseconds since epoch) of the last check (success or fail).
    last_check_ts: AtomicI64,
    /// Timestamp (microseconds since epoch) of the last completed full check.
    last_full_check_ts: AtomicI64,
    /// Total number of checks run.
    checks_total: AtomicU64,
    /// Total number of failures detected.
    failures_total: AtomicU64,
}

impl IntegrityCheckState {
    const fn new() -> Self {
        Self {
            last_ok_ts: AtomicI64::new(0),
            last_check_ts: AtomicI64::new(0),
            last_full_check_ts: AtomicI64::new(0),
            checks_total: AtomicU64::new(0),
            failures_total: AtomicU64::new(0),
        }
    }
}

fn state() -> &'static IntegrityCheckState {
    LAST_CHECK.get_or_init(IntegrityCheckState::new)
}

/// Snapshot of integrity check metrics for health reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityMetrics {
    pub last_ok_ts: i64,
    pub last_check_ts: i64,
    pub checks_total: u64,
    pub failures_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxIntegrityStatus {
    Healthy,
    Suspect,
    Broken,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxIntegrityVerdict {
    pub status: MailboxIntegrityStatus,
    pub metrics: IntegrityMetrics,
    pub check: Option<IntegrityCheckResult>,
    pub detail: String,
}

/// Get current integrity check metrics.
#[must_use]
pub fn integrity_metrics() -> IntegrityMetrics {
    let s = state();
    let runtime_failures = mcp_agent_mail_core::global_metrics()
        .db
        .integrity_failures_total
        .load();
    IntegrityMetrics {
        last_ok_ts: s.last_ok_ts.load(Ordering::Relaxed),
        last_check_ts: s.last_check_ts.load(Ordering::Relaxed),
        checks_total: s.checks_total.load(Ordering::Relaxed),
        failures_total: s
            .failures_total
            .load(Ordering::Relaxed)
            .saturating_add(runtime_failures),
    }
}

#[must_use]
pub fn inspect_mailbox_integrity(db_path: &Path, kind: CheckKind) -> MailboxIntegrityVerdict {
    let metrics_before = integrity_metrics();
    if db_path.as_os_str() == ":memory:" {
        return MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Skipped,
            metrics: metrics_before,
            check: None,
            detail: "In-memory database (integrity check skipped)".to_string(),
        };
    }

    let path_str = db_path.display().to_string();
    let conn = match crate::DbConn::open_file(&path_str) {
        Ok(conn) => conn,
        Err(error) => {
            return MailboxIntegrityVerdict {
                status: MailboxIntegrityStatus::Broken,
                metrics: metrics_before,
                check: None,
                detail: format!("Cannot open database for integrity check: {error}"),
            };
        }
    };

    match run_check(&conn, kind) {
        Ok(check) => MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Healthy,
            metrics: integrity_metrics(),
            detail: format!("{} passed", check.kind),
            check: Some(check),
        },
        Err(DbError::IntegrityCorruption { details, .. })
            if integrity_details_are_suspect(&details) =>
        {
            MailboxIntegrityVerdict {
                status: MailboxIntegrityStatus::Suspect,
                metrics: integrity_metrics(),
                detail: format!(
                    "{} reported benign/suspect findings: {}",
                    kind,
                    details.join("; ")
                ),
                check: Some(IntegrityCheckResult {
                    ok: false,
                    details,
                    duration_us: 0,
                    kind,
                }),
            }
        }
        Err(error) => MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Broken,
            metrics: integrity_metrics(),
            check: None,
            detail: error.to_string(),
        },
    }
}

#[must_use]
pub fn integrity_details_are_suspect(details: &[String]) -> bool {
    !details.is_empty()
        && details.iter().all(|detail| {
            let lower = detail.to_ascii_lowercase();
            lower == "ok"
                || lower.contains("never used")
                || lower.contains("unused")
                || lower.contains("wal without shm")
        })
}

/// Run `PRAGMA quick_check` on an open connection.
///
/// This is fast (typically <100ms) and catches most common corruption.
/// Suitable for startup validation.
pub fn quick_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Quick)
}

/// Run `PRAGMA integrity_check(1)` — stops after the first error.
///
/// Faster than a full check but provides less detail. Suitable for
/// periodic connection-recycle checks.
pub fn incremental_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Incremental)
}

/// Run a full `PRAGMA integrity_check`.
///
/// This scans the entire database and can take seconds on large databases.
/// Run on a dedicated connection, not from the pool hot path.
pub fn full_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, CheckKind::Full)
}

#[must_use]
pub const fn preferred_check_sql(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Quick => "SELECT quick_check FROM pragma_quick_check()",
        CheckKind::Incremental => "SELECT integrity_check FROM pragma_integrity_check() LIMIT 1",
        CheckKind::Full => "SELECT integrity_check FROM pragma_integrity_check()",
    }
}

#[must_use]
pub const fn fallback_check_sql(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Quick => "PRAGMA quick_check",
        CheckKind::Incremental => "PRAGMA integrity_check(1)",
        CheckKind::Full => "PRAGMA integrity_check",
    }
}

/// Run an integrity probe with a best-effort fallback from the table-valued
/// pragma form to the classic PRAGMA statement form.
///
/// Some SQLite-compatible engines in this workspace still do not implement the
/// table-valued `pragma_*` functions. Callers should use this helper instead of
/// hardcoding the preferred SQL so runtime probes, doctor flows, and recovery
/// all behave consistently.
pub fn probe_check_rows<F>(mut query: F, kind: CheckKind) -> Result<Vec<Row>, String>
where
    F: FnMut(&str) -> Result<Vec<Row>, String>,
{
    let preferred_sql = preferred_check_sql(kind);
    let fallback_sql = fallback_check_sql(kind);

    match query(preferred_sql) {
        Ok(rows) if !rows.is_empty() || preferred_sql == fallback_sql => Ok(rows),
        Ok(_) => query(fallback_sql).map_err(|fallback_error| {
            format!("preferred probe returned no rows and fallback probe errored: {fallback_error}")
        }),
        Err(preferred_error) => query(fallback_sql).map_err(|fallback_error| {
            format!(
                "preferred probe error: {preferred_error}; fallback probe error: {fallback_error}"
            )
        }),
    }
}

#[must_use]
pub fn extract_check_details(rows: &[Row], kind: CheckKind) -> Vec<String> {
    let primary_column = match kind {
        CheckKind::Quick => "quick_check",
        CheckKind::Incremental | CheckKind::Full => "integrity_check",
    };

    let mut details: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            if let Some(Value::Text(text)) = row.get_by_name(primary_column) {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.get_by_name("integrity_check") {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.get_by_name("quick_check") {
                Some(text.clone())
            } else if let Some(Value::Text(text)) = row.values().next() {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();

    if details.is_empty() {
        if rows.is_empty() {
            details.push(format!("{kind} returned no rows"));
        } else {
            details.push(format!(
                "{kind} returned {} row(s) but none had extractable text values",
                rows.len()
            ));
        }
    }

    details
}

#[must_use]
pub fn details_indicate_ok(details: &[String]) -> bool {
    details.len() == 1 && details[0].trim().eq_ignore_ascii_case("ok")
}

fn run_check(conn: &DbConn, kind: CheckKind) -> DbResult<IntegrityCheckResult> {
    let start = std::time::Instant::now();
    let rows: Vec<Row> = probe_check_rows(
        |sql| conn.query_sync(sql, &[]).map_err(|error| error.to_string()),
        kind,
    )
    .map_err(|error| DbError::Sqlite(format!("{kind} failed: {error}")))?;

    let duration_us =
        u64::try_from(start.elapsed().as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);

    evaluate_check_rows(&rows, kind, duration_us)
}

/// Evaluate integrity/quick-check pragma rows and update global integrity metrics.
///
/// Shared helper to keep integrity semantics consistent across all callers.
pub(crate) fn evaluate_check_details(
    details: Vec<String>,
    kind: CheckKind,
    duration_us: u64,
) -> DbResult<IntegrityCheckResult> {
    let ok = details_indicate_ok(&details);

    // Update global state.
    let s = state();
    let now = crate::now_micros();
    s.last_check_ts.store(now, Ordering::Relaxed);
    if kind == CheckKind::Full {
        s.last_full_check_ts.store(now, Ordering::Relaxed);
    }
    s.checks_total.fetch_add(1, Ordering::Relaxed);
    if ok {
        s.last_ok_ts.store(now, Ordering::Relaxed);
    } else {
        s.failures_total.fetch_add(1, Ordering::Relaxed);
    }

    let result = IntegrityCheckResult {
        ok,
        details,
        duration_us,
        kind,
    };

    if !ok {
        return Err(DbError::IntegrityCorruption {
            message: format!(
                "{kind} detected corruption ({duration_us}us): {}",
                result.details.join("; ")
            ),
            details: result.details,
        });
    }

    Ok(result)
}

/// Evaluate integrity/quick-check pragma rows and update global integrity metrics.
///
/// Shared helper to keep integrity semantics consistent across all callers.
pub fn evaluate_check_rows(
    rows: &[Row],
    kind: CheckKind,
    duration_us: u64,
) -> DbResult<IntegrityCheckResult> {
    let details = extract_check_details(rows, kind);
    evaluate_check_details(details, kind, duration_us)
}

/// Attempt recovery by checkpointing then copying the database file.
///
/// Returns the path of the clean copy on success.
pub fn attempt_vacuum_recovery(conn: &DbConn, original_path: &str) -> DbResult<String> {
    let recovery_path = format!("{original_path}.recovery");

    // Remove any leftover recovery file.
    cleanup_recovery_artifacts(&recovery_path);

    // Use PASSIVE checkpoint to flush what we can without modifying the
    // corrupt database aggressively. TRUNCATE could propagate WAL-resident
    // corruption into the main file. VACUUM on a corrupt DB risks partial
    // overwrite of the original before failing.
    let _ = conn.query_sync("PRAGMA wal_checkpoint(PASSIVE)", &[]);

    // Copy the database file as-is (preserving corruption evidence).
    std::fs::copy(original_path, &recovery_path)
        .map_err(|e| DbError::Sqlite(format!("copy recovery failed: {e}")))?;
    // Also copy WAL/SHM so the recovery copy has the full state.
    // These are best-effort (the files may not exist in rollback-journal mode),
    // but failures are logged because a missing WAL means recent writes are lost.
    if let Err(e) = std::fs::copy(
        format!("{original_path}-wal"),
        format!("{recovery_path}-wal"),
    ) {
        tracing::warn!(path = %original_path, error = %e, "WAL copy failed during recovery — recent writes may be missing");
    }
    if let Err(e) = std::fs::copy(
        format!("{original_path}-shm"),
        format!("{recovery_path}-shm"),
    ) {
        tracing::debug!(path = %original_path, error = %e, "SHM copy skipped during recovery");
    }

    // Verify the recovery copy is valid.
    let recovery_conn = DbConn::open_file(&recovery_path).map_err(|e| {
        cleanup_recovery_artifacts(&recovery_path);
        DbError::Sqlite(format!("failed to open recovery copy: {e}"))
    })?;

    match quick_check(&recovery_conn) {
        Ok(_) => Ok(recovery_path),
        Err(e) => {
            cleanup_recovery_artifacts(&recovery_path);
            Err(DbError::Internal(format!(
                "recovery copy also corrupt: {e}"
            )))
        }
    }
}

fn cleanup_recovery_artifacts(recovery_path: &str) {
    let _ = std::fs::remove_file(recovery_path);
    let _ = std::fs::remove_file(format!("{recovery_path}-wal"));
    let _ = std::fs::remove_file(format!("{recovery_path}-shm"));
}

/// Check whether enough time has elapsed since the last full check
/// to warrant running another one.
///
/// Returns `true` if `interval_hours` have elapsed since the last full check,
/// or if no full check has ever been run.
#[must_use]
pub fn is_full_check_due(interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return false;
    }
    let s = state();
    let last = s.last_full_check_ts.load(Ordering::Relaxed);
    if last == 0 {
        return true;
    }
    let now = crate::now_micros();
    let elapsed_hours = u64::try_from((now - last).max(0)).unwrap_or(0) / (3_600 * 1_000_000);
    elapsed_hours >= interval_hours
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static TEST_STATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn open_test_db() -> DbConn {
        let conn = DbConn::open_memory().expect("open memory db");
        conn.execute_raw("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create table");
        conn
    }

    fn set_state_for_tests(
        last_ok_ts: i64,
        last_check_ts: i64,
        last_full_check_ts: i64,
        checks_total: u64,
        failures_total: u64,
    ) {
        let s = state();
        s.last_ok_ts.store(last_ok_ts, Ordering::Relaxed);
        s.last_check_ts.store(last_check_ts, Ordering::Relaxed);
        s.last_full_check_ts
            .store(last_full_check_ts, Ordering::Relaxed);
        s.checks_total.store(checks_total, Ordering::Relaxed);
        s.failures_total.store(failures_total, Ordering::Relaxed);
    }

    #[test]
    fn quick_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = quick_check(&conn).expect("quick_check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Quick);
        assert!(result.duration_us < 1_000_000); // < 1s
    }

    #[test]
    fn incremental_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = incremental_check(&conn).expect("incremental check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Incremental);
    }

    #[test]
    fn full_check_passes_on_healthy_db() {
        let conn = open_test_db();
        let result = full_check(&conn).expect("full check should pass");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
        assert_eq!(result.kind, CheckKind::Full);
    }

    #[test]
    fn check_kind_display() {
        assert_eq!(CheckKind::Quick.to_string(), "quick_check");
        assert_eq!(CheckKind::Incremental.to_string(), "integrity_check(1)");
        assert_eq!(CheckKind::Full.to_string(), "integrity_check");
    }

    #[test]
    fn integrity_metrics_tracks_checks() {
        let conn = open_test_db();
        let before = integrity_metrics();
        let before_total = before.checks_total;

        let _ = quick_check(&conn);
        let _ = full_check(&conn);

        let after = integrity_metrics();
        assert!(
            after.checks_total >= before_total + 2,
            "checks_total should increase by at least 2"
        );
        assert!(after.last_ok_ts > 0, "last_ok_ts should be set");
        assert!(after.last_check_ts > 0, "last_check_ts should be set");
    }

    #[test]
    fn is_full_check_due_when_never_run() {
        // This test checks the logic; the global state may have been
        // modified by other tests, but interval=0 should always be false.
        assert!(!is_full_check_due(0), "interval=0 means disabled");
    }

    #[test]
    fn integrity_metrics_serializable() {
        let m = integrity_metrics();
        let json = serde_json::to_value(&m).expect("serialize IntegrityMetrics");
        assert!(json.get("last_ok_ts").is_some());
        assert!(json.get("last_check_ts").is_some());
        assert!(json.get("checks_total").is_some());
        assert!(json.get("failures_total").is_some());
    }

    #[test]
    fn check_kind_equality() {
        assert_eq!(CheckKind::Quick, CheckKind::Quick);
        assert_ne!(CheckKind::Quick, CheckKind::Incremental);
        assert_ne!(CheckKind::Incremental, CheckKind::Full);
    }

    #[test]
    fn integrity_check_result_clone() {
        let conn = open_test_db();
        let result = quick_check(&conn).expect("quick_check");
        let cloned = result.clone();
        assert_eq!(cloned.ok, result.ok);
        assert_eq!(cloned.details, result.details);
        assert_eq!(cloned.kind, result.kind);
    }

    #[test]
    fn extract_check_details_empty_rows_are_not_ok() {
        let details = extract_check_details(&[], CheckKind::Quick);
        assert_eq!(details, vec!["quick_check returned no rows"]);
        assert!(
            !details_indicate_ok(&details),
            "empty integrity probes must not be normalized to success"
        );
    }

    #[test]
    fn evaluate_check_rows_empty_rows_reports_corruption() {
        let err = evaluate_check_rows(&[], CheckKind::Incremental, 0)
            .expect_err("empty integrity rows should fail");
        assert!(
            err.to_string().contains("returned no rows"),
            "unexpected empty-row verdict: {err}"
        );
    }

    #[test]
    fn probe_check_rows_falls_back_after_preferred_error() {
        let mut calls = Vec::new();
        let rows = probe_check_rows(
            |sql| {
                calls.push(sql.to_string());
                if sql == preferred_check_sql(CheckKind::Quick) {
                    Err("table-valued pragma unsupported".to_string())
                } else {
                    Ok(Vec::new())
                }
            },
            CheckKind::Quick,
        )
        .expect("fallback probe should succeed");

        assert!(
            rows.is_empty(),
            "test fallback intentionally returns empty rows"
        );
        assert_eq!(
            calls,
            vec![
                preferred_check_sql(CheckKind::Quick).to_string(),
                fallback_check_sql(CheckKind::Quick).to_string()
            ]
        );
    }

    #[test]
    fn probe_check_rows_falls_back_after_empty_preferred_rows() {
        let mut calls = Vec::new();
        let rows = probe_check_rows(
            |sql| {
                calls.push(sql.to_string());
                Ok(Vec::new())
            },
            CheckKind::Incremental,
        )
        .expect("fallback probe should succeed");

        assert!(
            rows.is_empty(),
            "test fallback intentionally returns empty rows"
        );
        assert_eq!(
            calls,
            vec![
                preferred_check_sql(CheckKind::Incremental).to_string(),
                fallback_check_sql(CheckKind::Incremental).to_string()
            ]
        );
    }

    #[test]
    fn is_full_check_due_zero_interval_always_false() {
        // Regardless of state, interval=0 means disabled
        assert!(!is_full_check_due(0));
    }

    #[test]
    fn integrity_check_result_debug() {
        let result = IntegrityCheckResult {
            ok: true,
            details: vec!["ok".to_string()],
            duration_us: 42,
            kind: CheckKind::Quick,
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("ok: true"));
        assert!(debug.contains("Quick"));
    }

    #[test]
    fn quick_and_incremental_both_pass_on_same_db() {
        let conn = open_test_db();
        // Insert some data
        conn.execute_raw("INSERT INTO test (id, name) VALUES (1, 'alpha')")
            .expect("insert");
        conn.execute_raw("INSERT INTO test (id, name) VALUES (2, 'beta')")
            .expect("insert");

        let qr = quick_check(&conn).expect("quick_check");
        assert!(qr.ok);
        let ir = incremental_check(&conn).expect("incremental_check");
        assert!(ir.ok);
        let fr = full_check(&conn).expect("full_check");
        assert!(fr.ok);
    }

    // ── br-3h13: Additional integrity.rs test coverage ─────────────

    #[test]
    fn quick_check_with_populated_db() {
        let conn = open_test_db();
        for i in 0..100 {
            conn.execute_raw(&format!(
                "INSERT INTO test (id, name) VALUES ({i}, 'item{i}')"
            ))
            .expect("insert");
        }
        let result = quick_check(&conn).expect("quick_check on populated DB");
        assert!(result.ok);
        assert_eq!(result.details, vec!["ok"]);
    }

    #[test]
    fn full_check_with_multiple_tables() {
        let conn = open_test_db();
        conn.execute_raw("CREATE TABLE other (val REAL)")
            .expect("create other");
        conn.execute_raw("INSERT INTO other VALUES (3.14)")
            .expect("insert");
        let result = full_check(&conn).expect("full check with multiple tables");
        assert!(result.ok);
    }

    #[test]
    fn integrity_metrics_failures_start_at_zero_or_above() {
        let m = integrity_metrics();
        // failures_total is cumulative from all tests, but should be non-negative
        assert!(m.failures_total < 1000, "unexpected failure count");
    }

    #[test]
    fn integrity_check_result_debug_with_failure_details() {
        let result = IntegrityCheckResult {
            ok: false,
            details: vec![
                "*** in database main ***".to_string(),
                "row 5 missing from index idx_test_name".to_string(),
            ],
            duration_us: 12345,
            kind: CheckKind::Full,
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("ok: false"));
        assert!(debug.contains("Full"));
        assert!(debug.contains("12345"));
    }

    #[test]
    fn check_kind_all_display_values_are_distinct() {
        let displays: Vec<String> = [CheckKind::Quick, CheckKind::Incremental, CheckKind::Full]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(displays.len(), 3);
        // All must be unique
        let mut sorted = displays;
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            3,
            "all CheckKind display values must be distinct"
        );
    }

    #[test]
    fn integrity_metrics_serde_has_all_fields() {
        let m = integrity_metrics();
        let json = serde_json::to_value(&m).expect("serialize");
        let obj = json.as_object().expect("should be object");
        assert_eq!(
            obj.len(),
            4,
            "IntegrityMetrics should have exactly 4 fields"
        );
        for key in &[
            "last_ok_ts",
            "last_check_ts",
            "checks_total",
            "failures_total",
        ] {
            assert!(obj.contains_key(*key), "missing field: {key}");
        }
    }

    #[test]
    fn is_full_check_due_with_large_interval_is_false_after_recent_check() {
        // Run a full check to update last_full_check_ts to now.
        let conn = open_test_db();
        let _ = full_check(&conn);
        // interval of 1 billion hours should NOT be due
        assert!(!is_full_check_due(1_000_000_000));
    }

    #[test]
    fn is_full_check_due_ignores_recent_non_full_checks() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let now = crate::now_micros();
        set_state_for_tests(now, now, now - 25 * 3_600 * 1_000_000, 10, 0);
        assert!(
            is_full_check_due(24),
            "recent quick/incremental checks must not hide an overdue full scan"
        );
    }

    #[test]
    fn integrity_metrics_include_runtime_corruption_failures() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let metrics = mcp_agent_mail_core::global_metrics();
        let runtime_before = metrics.db.integrity_failures_total.load();
        let s = state();
        let state_before = (
            s.last_ok_ts.load(Ordering::Relaxed),
            s.last_check_ts.load(Ordering::Relaxed),
            s.last_full_check_ts.load(Ordering::Relaxed),
            s.checks_total.load(Ordering::Relaxed),
            s.failures_total.load(Ordering::Relaxed),
        );

        set_state_for_tests(0, 0, 0, 0, 0);
        metrics
            .db
            .integrity_failures_total
            .store(runtime_before.saturating_add(1));

        let snapshot = integrity_metrics();
        assert_eq!(
            snapshot.failures_total,
            runtime_before.saturating_add(1),
            "runtime corruption failures should surface in integrity metrics"
        );

        metrics.db.integrity_failures_total.store(runtime_before);
        set_state_for_tests(
            state_before.0,
            state_before.1,
            state_before.2,
            state_before.3,
            state_before.4,
        );
    }

    #[test]
    fn integrity_metrics_add_runtime_and_pragma_failures() {
        let _guard = TEST_STATE_LOCK.lock().unwrap();
        let metrics = mcp_agent_mail_core::global_metrics();
        let runtime_before = metrics.db.integrity_failures_total.load();
        let s = state();
        let state_before = (
            s.last_ok_ts.load(Ordering::Relaxed),
            s.last_check_ts.load(Ordering::Relaxed),
            s.last_full_check_ts.load(Ordering::Relaxed),
            s.checks_total.load(Ordering::Relaxed),
            s.failures_total.load(Ordering::Relaxed),
        );

        set_state_for_tests(0, 0, 0, 0, 3);
        metrics
            .db
            .integrity_failures_total
            .store(runtime_before.saturating_add(7));

        let snapshot = integrity_metrics();
        assert_eq!(
            snapshot.failures_total,
            runtime_before.saturating_add(10),
            "integrity metrics should include both PRAGMA-detected and runtime failures"
        );

        metrics.db.integrity_failures_total.store(runtime_before);
        set_state_for_tests(
            state_before.0,
            state_before.1,
            state_before.2,
            state_before.3,
            state_before.4,
        );
    }

    #[test]
    fn integrity_check_result_clone_preserves_all_fields() {
        let original = IntegrityCheckResult {
            ok: false,
            details: vec!["error1".into(), "error2".into()],
            duration_us: 99999,
            kind: CheckKind::Incremental,
        };
        let cloned = original.clone();
        assert!(!cloned.ok);
        assert_eq!(cloned.details.len(), 2);
        // Use original after clone to prove independent copy.
        assert!(!original.ok);
        assert_eq!(cloned.details[0], "error1");
        assert_eq!(cloned.duration_us, 99999);
        assert_eq!(cloned.kind, CheckKind::Incremental);
    }

    #[test]
    fn vacuum_recovery_on_healthy_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let db_str = db_path.to_str().expect("path str");

        let conn = DbConn::open_file(db_str).expect("open db");
        conn.execute_raw("CREATE TABLE foo (id INTEGER PRIMARY KEY)")
            .expect("create table");
        conn.execute_raw("INSERT INTO foo VALUES (1)")
            .expect("insert");

        let recovery_path = attempt_vacuum_recovery(&conn, db_str).expect("vacuum recovery");
        assert!(
            std::path::Path::new(&recovery_path).exists(),
            "recovery file should exist"
        );

        // Verify recovery copy has data.
        let recovery_conn = DbConn::open_file(&recovery_path).expect("open recovery");
        let rows: Vec<Row> = recovery_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM foo", &[])
            .expect("query");
        let cnt = rows
            .first()
            .and_then(|r| match r.get_by_name("cnt") {
                Some(Value::BigInt(n)) => Some(*n),
                Some(Value::Int(n)) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(cnt, 1, "recovery copy should have the data");
    }

    #[test]
    fn cleanup_recovery_artifacts_removes_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recovery = dir.path().join("test.db.recovery");
        std::fs::write(&recovery, b"db").expect("write recovery db");
        std::fs::write(format!("{}-wal", recovery.display()), b"wal").expect("write recovery wal");
        std::fs::write(format!("{}-shm", recovery.display()), b"shm").expect("write recovery shm");

        cleanup_recovery_artifacts(recovery.to_str().expect("recovery path"));

        assert!(!recovery.exists(), "recovery db should be removed");
        assert!(
            !dir.path().join("test.db.recovery-wal").exists(),
            "recovery wal should be removed"
        );
        assert!(
            !dir.path().join("test.db.recovery-shm").exists(),
            "recovery shm should be removed"
        );
    }
}
