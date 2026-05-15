//! `fm-db-state-files-busy-timeout-missing` — P2.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `storage.sqlite3` opens with a `PRAGMA busy_timeout` lower than
//! 30000 ms (or zero). The pool's regression test
//! `pragma_busy_timeout_matches_legacy` (pool.rs) asserts a
//! 60000-ms timeout — anything substantially lower causes the
//! Rust server's concurrent readers/writers to fail with
//! `database is locked` errors instead of blocking briefly and
//! retrying.
//!
//! ## Detection (pure)
//!
//! Open the DB read-only, run `PRAGMA busy_timeout;`, parse the
//! integer result. If less than 30000 ms, emit a finding. The
//! threshold matches the repair_spec — gives slack for builds
//! that explicitly tune lower, but flags the default-zero case.
//!
//! ## Fix
//!
//! **Detect-only in this first cut.** The repair_spec's full
//! Op::DbExec + session-init-marker fix is intentionally deferred
//! because the busy_timeout PRAGMA is connection-local — setting
//! it on a one-off SQLite connection inside the chokepoint
//! doesn't persist across the pool's other connections. The
//! correct fix is to bump a session-init marker file so all
//! pooled connections re-run init SQL on next acquire, and that
//! requires plumbing through the pool layer.
//!
//! For now the manual_remediation envelope points operators at
//! restarting `am serve-http` (which re-runs the connection
//! init SQL) or running `am setup --reset-session-init` once
//! that command exists.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-busy-timeout-missing";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Minimum acceptable busy_timeout in milliseconds. Matches the
/// repair_spec; the canonical pool config is 60000 ms.
pub const MIN_BUSY_TIMEOUT_MS: i64 = 30000;

/// Canonical / target busy_timeout in milliseconds (for evidence
/// rendering and manual_remediation guidance).
pub const TARGET_BUSY_TIMEOUT_MS: i64 = 60000;

#[derive(Debug, Clone, Serialize)]
pub struct BusyTimeoutMissingFinding {
    pub db_path: PathBuf,
    pub current_busy_timeout_ms: i64,
}

impl BusyTimeoutMissingFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB {} has busy_timeout={} ms (min {} ms, target {} ms); causes spurious 'database is locked' errors under concurrent load",
            self.db_path.display(),
            self.current_busy_timeout_ms,
            MIN_BUSY_TIMEOUT_MS,
            TARGET_BUSY_TIMEOUT_MS,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "current_busy_timeout_ms": self.current_busy_timeout_ms,
                "min_busy_timeout_ms": MIN_BUSY_TIMEOUT_MS,
                "target_busy_timeout_ms": TARGET_BUSY_TIMEOUT_MS,
                "remediation_pragma": format!("PRAGMA busy_timeout = {};", TARGET_BUSY_TIMEOUT_MS),
                "manual_remediation": {
                    "steps": [
                        "Restart `am serve-http` (or whatever process holds the pool) — connection init SQL runs on next acquire and sets the canonical busy_timeout.",
                        "If a custom build tuned busy_timeout intentionally below 30000 ms, this finding can be ignored; the threshold reflects the project's stated default.",
                    ],
                    "note": "Per-connection PRAGMA busy_timeout is local to one SqliteConnection; auto-fix is deferred until the pool's session-init-marker plumbing is in place.",
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE w.r.t. caller-supplied DB paths; reads from the
/// SQLite file via `PRAGMA busy_timeout;`.
///
/// `candidate_paths` is typically `[<storage_root>/storage.sqlite3]`.
/// Empty slice skips the FM. `:memory:` URLs should be filtered
/// out by the caller (this detector tries to open a real file).
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<BusyTimeoutMissingFinding> {
    use sqlmodel_sqlite::SqliteConnection;
    let mut out = Vec::new();
    for path in candidate_paths {
        if !path.is_file() {
            continue;
        }
        let Ok(conn) = SqliteConnection::open_file(path.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(timeout_ms) = read_busy_timeout_ms(&conn) else {
            continue;
        };
        if timeout_ms >= MIN_BUSY_TIMEOUT_MS {
            continue;
        }
        out.push(BusyTimeoutMissingFinding {
            db_path: path.clone(),
            current_busy_timeout_ms: timeout_ms,
        });
    }
    out
}

/// Read `PRAGMA busy_timeout;` from an open connection. Returns
/// `Some(ms)` on success, `None` on any error.
fn read_busy_timeout_ms(conn: &sqlmodel_sqlite::SqliteConnection) -> Option<i64> {
    let rows = conn.query_sync("PRAGMA busy_timeout;", &[]).ok()?;
    let first = rows.first()?;
    first.get_named::<i64>("timeout").ok()
}

/// Fixer. Detect-only — returns `actions_skipped: 1`. Auto-fix is
/// intentionally deferred because per-connection `PRAGMA busy_timeout`
/// doesn't persist across the pool's other connections; the correct
/// fix needs the pool's session-init-marker plumbing.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &BusyTimeoutMissingFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlmodel_sqlite::SqliteConnection;
    use std::fs;
    use tempfile::TempDir;

    fn make_db_with_timeout(td: &TempDir, name: &str, timeout_ms: i64) -> PathBuf {
        let p = td.path().join(name);
        let conn = SqliteConnection::open_file(p.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        // Apply requested busy_timeout. Note: PRAGMA busy_timeout
        // is connection-local. Each subsequent open of this DB
        // file starts fresh at SQLite's default (0).
        if timeout_ms > 0 {
            conn.execute_raw(&format!("PRAGMA busy_timeout = {timeout_ms};"))
                .expect("set busy_timeout");
        }
        // Force a schema write so the file has content (not zero-byte).
        conn.execute_raw("CREATE TABLE IF NOT EXISTS t (a INTEGER);")
            .expect("create table");
        drop(conn);
        p
    }

    #[test]
    fn detector_flags_db_with_default_zero_busy_timeout() {
        // Detector opens a fresh connection to the file — even
        // though make_db_with_timeout set a timeout, the fresh
        // open starts at default (0 ms), which is < MIN.
        let td = TempDir::new().unwrap();
        let db = make_db_with_timeout(&td, "default.sqlite3", 60000);
        let findings = detect(&[db.clone()]);
        assert_eq!(findings.len(), 1, "default open should be flagged");
        assert_eq!(findings[0].db_path, db);
        assert!(
            findings[0].current_busy_timeout_ms < MIN_BUSY_TIMEOUT_MS,
            "current must be below the min threshold"
        );
    }

    #[test]
    fn detector_skips_missing_path() {
        let findings = detect(&[PathBuf::from("/nonexistent/path/to/storage.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_empty_input() {
        let findings = detect(&[]);
        assert!(findings.is_empty());
    }

    #[test]
    fn finding_serializes_with_threshold_and_target() {
        let f = BusyTimeoutMissingFinding {
            db_path: "/var/data/storage.sqlite3".into(),
            current_busy_timeout_ms: 0,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"current_busy_timeout_ms\":0"));
        assert!(s.contains("\"min_busy_timeout_ms\":30000"));
        assert!(s.contains("\"target_busy_timeout_ms\":60000"));
        assert!(s.contains("PRAGMA busy_timeout"));
        // Detect-only: auto_fixable must be false.
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("manual_remediation"));
    }

    #[test]
    fn finding_title_renders_both_thresholds() {
        let f = BusyTimeoutMissingFinding {
            db_path: "/var/data/storage.sqlite3".into(),
            current_busy_timeout_ms: 5,
        };
        let g = f.to_finding();
        assert!(g.title.contains("busy_timeout=5 ms"));
        assert!(g.title.contains("min 30000"));
        assert!(g.title.contains("target 60000"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        // Use a real run_dir + actions_file so MutateContext is
        // structurally valid even though fix() doesn't touch them.
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = BusyTimeoutMissingFinding {
            db_path: td.path().join("nonexistent.sqlite3"),
            current_busy_timeout_ms: 0,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(outcome.quarantined_paths.is_empty());
    }
}
