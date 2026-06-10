//! `fm-db-state-files-wal-mode-disabled` — P1.
//!
//! **Subsystem**: db_state_files (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! Agent Mail's `storage.sqlite3` should run in WAL journal mode
//! for concurrent reader+writer durability. Without WAL the DB
//! falls back to the default `rollback` (or `delete`) journal,
//! which means readers block writers and writers acquire an
//! exclusive lock for the entire transaction — fine for single-
//! process Python, but the Rust server pool issues concurrent
//! queries that hit `database is locked` errors without WAL.
//!
//! ## Detection (pure function)
//!
//! Open the DB at `db_path` read-only with URI `?immutable=1`
//! (no WAL/SHM sidecar creation or journal replay), run
//! `PRAGMA journal_mode;`, parse the result. If the value is
//! anything other than `wal` (case-insensitive), emit a finding.
//!
//! ## Fix (`Op::DbExec` — new pattern)
//!
//! `mutate(ctx, db_path, Op::DbExec { sql: "PRAGMA journal_mode=WAL;" })`.
//! Pass-34 wired the chokepoint to open a SqliteConnection at
//! `path`, run the SQL via `execute_raw`, and close — with file-
//! level byte backup before exec. The before/after hashes record
//! the on-disk DB file change; undo restores the pre-WAL file
//! byte-identical.
//!
//! Note on WAL/SHM siblings: switching INTO WAL mode creates
//! `.sqlite3-wal` and `.sqlite3-shm` files alongside the main DB.
//! Those are NOT backed up by the chokepoint (it operates at
//! file level on `path` only). Undo restores the main DB to its
//! pre-WAL state; SQLite is robust to stale WAL/SHM artifacts
//! and recovers on next open.
//!
//! ## Reversibility
//!
//! Standard via `am doctor undo <run-id>`: the chokepoint's
//! file-level backup is restored. The DB journal_mode reverts
//! to whatever the original file recorded.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-wal-mode-disabled";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Canonical WAL mode value (lowercase per SQLite's PRAGMA output).
pub const TARGET_JOURNAL_MODE: &str = "wal";

#[derive(Debug, Clone, Serialize)]
pub struct WalModeDisabledFinding {
    pub db_path: PathBuf,
    pub current_journal_mode: String,
}

impl WalModeDisabledFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB {} has journal_mode='{}' (target 'wal'); causes reader/writer lock contention",
            self.db_path.display(),
            self.current_journal_mode
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "current_journal_mode": self.current_journal_mode,
                "target_journal_mode": TARGET_JOURNAL_MODE,
                "remediation_pragma": "PRAGMA journal_mode=WAL;",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE w.r.t. caller-supplied DB paths; reads from the
/// SQLite file via `PRAGMA journal_mode;`.
///
/// `candidate_paths` is typically `[<storage_root>/storage.sqlite3]`.
/// Empty slice skips the FM. `:memory:` URLs should be filtered
/// out by the caller (this detector tries to open a real file).
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<WalModeDisabledFinding> {
    let mut out = Vec::new();
    for path in candidate_paths {
        if !path.is_file() {
            continue;
        }
        let Ok(conn) = super::open_immutable_sqlite(path) else {
            continue;
        };
        let mode = match read_journal_mode(&conn) {
            Some(m) => m,
            None => continue,
        };
        if mode.eq_ignore_ascii_case(TARGET_JOURNAL_MODE) {
            continue; // already WAL → healthy
        }
        out.push(WalModeDisabledFinding {
            db_path: path.clone(),
            current_journal_mode: mode,
        });
    }
    out
}

/// Read `PRAGMA journal_mode;` from an open connection. Returns
/// `Some(lowercased_mode)` on success, `None` on any error.
///
/// sqlmodel-sqlite's `execute_raw` returns `Result<(), Error>` and
/// doesn't expose query results. For the PRAGMA we need a query
/// result, so use `query_sync` instead.
fn read_journal_mode(conn: &sqlmodel_sqlite::SqliteConnection) -> Option<String> {
    let rows = conn.query_sync("PRAGMA journal_mode;", &[]).ok()?;
    let first = rows.first()?;
    // The PRAGMA result has one column. Fetch it as a string.
    first
        .get_named::<String>("journal_mode")
        .ok()
        .map(|s| s.to_lowercase())
}

/// Fixer. Routes through `mutate()` with `Op::DbExec`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &WalModeDisabledFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if !finding.db_path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    mutate(
        ctx,
        &finding.db_path,
        Op::DbExec {
            sql: "PRAGMA journal_mode=WAL;".to_string(),
        },
    )?;
    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use crate::doctor::runs::scaffold_run_dir;
    use sqlmodel_sqlite::SqliteConnection;
    use std::fs;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir: run_dir.clone(),
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    fn make_db(td: &TempDir, name: &str, journal_mode: &str) -> PathBuf {
        let p = td.path().join(name);
        let conn = SqliteConnection::open_file(p.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw(&format!("PRAGMA journal_mode={journal_mode};"))
            .expect("set initial journal_mode");
        // Force at least one schema write so the file has content.
        conn.execute_raw("CREATE TABLE IF NOT EXISTS t (a INTEGER);")
            .expect("create table");
        drop(conn);
        p
    }

    #[test]
    fn detector_returns_empty_for_wal_db() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td, "wal.sqlite3", "WAL");
        let findings = detect(&[db]);
        assert!(findings.is_empty(), "WAL DB must not flag");
    }

    #[test]
    fn detector_flags_delete_journal_mode_db() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td, "delete.sqlite3", "DELETE");
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].current_journal_mode.to_lowercase(), "delete");
    }

    #[test]
    fn detector_skips_missing_file() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn fixer_switches_to_wal_via_mutate() {
        let td = TempDir::new().unwrap();
        let db = make_db(&td, "to_wal.sqlite3", "DELETE");
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-14T01-00-00Z__wal_enable");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        // Verify WAL mode is now active.
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let mode = read_journal_mode(&conn).unwrap();
        drop(conn);
        assert_eq!(mode, "wal", "post-fix mode must be wal (got {mode})");
    }

    #[test]
    fn fixer_skips_when_db_vanished() {
        let td = TempDir::new().unwrap();
        let finding = WalModeDisabledFinding {
            db_path: td.path().join("gone.sqlite3"),
            current_journal_mode: "delete".into(),
        };
        let ctx = ctx_for(&td, "2026-05-14T01-00-00Z__wal_vanish");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn finding_severity_is_p1_and_op_pattern_is_db_exec() {
        let f = WalModeDisabledFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            current_journal_mode: "delete".into(),
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P1");
        assert_eq!(g.subsystem, "db_state_files");
        assert!(g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("journal_mode=WAL"));
    }
}
