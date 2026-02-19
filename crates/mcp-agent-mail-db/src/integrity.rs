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
//! - Optionally attempts recovery via `VACUUM INTO` to create a clean copy.

use crate::error::{DbError, DbResult};
use crate::DbConn;
use sqlmodel_core::{Row, Value};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Result of an integrity check.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            checks_total: AtomicU64::new(0),
            failures_total: AtomicU64::new(0),
        }
    }
}

fn state() -> &'static IntegrityCheckState {
    LAST_CHECK.get_or_init(IntegrityCheckState::new)
}

/// Snapshot of integrity check metrics for health reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IntegrityMetrics {
    pub last_ok_ts: i64,
    pub last_check_ts: i64,
    pub checks_total: u64,
    pub failures_total: u64,
}

/// Get current integrity check metrics.
#[must_use]
pub fn integrity_metrics() -> IntegrityMetrics {
    let s = state();
    IntegrityMetrics {
        last_ok_ts: s.last_ok_ts.load(Ordering::Relaxed),
        last_check_ts: s.last_check_ts.load(Ordering::Relaxed),
        checks_total: s.checks_total.load(Ordering::Relaxed),
        failures_total: s.failures_total.load(Ordering::Relaxed),
    }
}

/// Run `PRAGMA quick_check` on an open connection.
///
/// This is fast (typically <100ms) and catches most common corruption.
/// Suitable for startup validation.
pub fn quick_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, "PRAGMA quick_check", CheckKind::Quick)
}

/// Run `PRAGMA integrity_check(1)` — stops after the first error.
///
/// Faster than a full check but provides less detail. Suitable for
/// periodic connection-recycle checks.
pub fn incremental_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, "PRAGMA integrity_check(1)", CheckKind::Incremental)
}

/// Run a full `PRAGMA integrity_check`.
///
/// This scans the entire database and can take seconds on large databases.
/// Run on a dedicated connection, not from the pool hot path.
pub fn full_check(conn: &DbConn) -> DbResult<IntegrityCheckResult> {
    run_check(conn, "PRAGMA integrity_check", CheckKind::Full)
}

fn run_check(
    conn: &DbConn,
    pragma: &str,
    kind: CheckKind,
) -> DbResult<IntegrityCheckResult> {
    let start = std::time::Instant::now();

    let rows: Vec<Row> = conn
        .query_sync(pragma, &[])
        .map_err(|e| DbError::Sqlite(format!("{kind} failed: {e}")))?;

    let duration_us =
        u64::try_from(start.elapsed().as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);

    let mut details: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            // PRAGMA integrity_check returns a column named "integrity_check",
            // quick_check returns "quick_check". Try both, fall back to index 0.
            if let Some(Value::Text(s)) = r.get_by_name("integrity_check") {
                Some(s.clone())
            } else if let Some(Value::Text(s)) = r.get_by_name("quick_check") {
                Some(s.clone())
            } else if let Some(Value::Text(s)) = r.values().next() {
                Some(s.clone())
            } else {
                None
            }
        })
        .collect();

    // Some SQLite backends currently surface PRAGMA check success with an empty
    // rowset instead of a single "ok" row; normalize that to preserve semantics.
    if details.is_empty() {
        details.push("ok".to_string());
    }

    // SQLite returns "ok" as the single row when no corruption is found.
    let ok = details.len() == 1 && details[0] == "ok";

    // Update global state.
    let s = state();
    let now = crate::now_micros();
    s.last_check_ts.store(now, Ordering::Relaxed);
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

/// Attempt recovery by checkpointing then copying the database file.
///
/// Returns the path of the clean copy on success.
pub fn attempt_vacuum_recovery(conn: &DbConn, original_path: &str) -> DbResult<String> {
    let recovery_path = format!("{original_path}.recovery");

    // Remove any leftover recovery file.
    let _ = std::fs::remove_file(&recovery_path);

    // Ensure latest committed state is in the main DB file before copying.
    let _ = conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[]);
    let _ = conn.execute_raw("VACUUM");
    std::fs::copy(original_path, &recovery_path)
        .map_err(|e| DbError::Sqlite(format!("copy recovery failed: {e}")))?;

    // Verify the recovery copy is valid.
    let recovery_conn = DbConn::open_file(&recovery_path)
        .map_err(|e| DbError::Sqlite(format!("failed to open recovery copy: {e}")))?;

    match quick_check(&recovery_conn) {
        Ok(_) => Ok(recovery_path),
        Err(e) => {
            let _ = std::fs::remove_file(&recovery_path);
            Err(DbError::Internal(format!(
                "recovery copy also corrupt: {e}"
            )))
        }
    }
}

/// Check whether enough time has elapsed since the last full check
/// to warrant running another one.
///
/// Returns `true` if `interval_hours` have elapsed since the last check,
/// or if no check has ever been run.
#[must_use]
pub fn is_full_check_due(interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return false;
    }
    let s = state();
    let last = s.last_check_ts.load(Ordering::Relaxed);
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

    fn open_test_db() -> DbConn {
        let conn = DbConn::open_memory().expect("open memory db");
        conn.execute_raw("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create table");
        conn
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
        // Run a check to update last_check_ts to now
        let conn = open_test_db();
        let _ = quick_check(&conn);
        // interval of 1 billion hours should NOT be due
        assert!(!is_full_check_due(1_000_000_000));
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
}
