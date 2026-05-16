//! `fm-db-state-files-orphan-foreign-key-rows` — P1 detect-only.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `PRAGMA foreign_key_check` returns one or more rows. Each row
//! represents a child-table row whose FK references a parent that
//! no longer exists — e.g., a `message_recipients` row pointing at
//! an `agents` row that has since been deleted, or a
//! `file_reservations` row pointing at a deleted `projects` row.
//!
//! FK orphans are distinct from page-level corruption
//! (`integrity_page_malformed`) and don't trip the cheap-default
//! detectors. They typically arise from:
//!
//! - `am doctor reconstruct` writing a fresh DB that pruned a
//!   parent without cascading the child references;
//! - manual `DELETE FROM agents WHERE name = ...` (or similar)
//!   issued while `PRAGMA foreign_keys = OFF`;
//! - a partial restore where the parent table came from a fresher
//!   backup than the child table.
//!
//! Orphans silently break downstream queries: `fetch_inbox` may
//! return rows whose sender no longer exists, `whois` errors when
//! resolving them, FTS V3 indexing skips them, and the pre-commit
//! guard can be confused by stale reservation rows. The doctor
//! surfaces them so an operator can pick a recovery path.
//!
//! ## Detection (pure)
//!
//! Opens each candidate DB read-only with URI `?immutable=1`
//! (matches the FM `integrity_page_malformed` pattern: no -shm
//! creation, no locking) and runs:
//!
//! ```sql
//! PRAGMA foreign_keys = ON;
//! PRAGMA foreign_key_check;
//! ```
//!
//! Each result row is `(table, rowid, parent, fkid)`. The
//! detector groups by parent table and reports the total count
//! plus a bounded sample (10 rows) so a multi-million-orphan
//! incident doesn't blow up the report.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair spec calls for
//! `Op::DbExec` quarantine to `<table>_orphans` sibling tables;
//! that's substantial additional plumbing (new schema migration
//! to create the sibling tables, per-orphan SQL dump for
//! forensics, idempotence verification). Until then, manual
//! remediation routes operators to `am doctor reconstruct` for
//! a whole-DB rebuild from the git archive, OR per-table SQL
//! cleanup if the operator is confident in the orphan set.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-db-state-files-orphan-foreign-key-rows";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Cap on the per-finding orphan sample (avoids unbounded growth
/// when a mass-deletion blew out thousands of FK references).
const SAMPLE_CAP: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct OrphanRow {
    pub child_table: String,
    pub child_rowid: i64,
    pub parent_table: String,
    pub fkid: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanForeignKeyRowsFinding {
    pub db_path: PathBuf,
    pub total_orphans: usize,
    /// Per-parent-table counts (e.g., `agents → 3, projects → 1`).
    /// Useful for operators deciding which recovery path applies.
    pub by_parent_table: BTreeMap<String, usize>,
    /// Bounded sample (≤ SAMPLE_CAP rows) for operator triage.
    pub sample: Vec<OrphanRow>,
}

impl OrphanForeignKeyRowsFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB {} has {} orphan FK row(s) across {} parent table(s)",
            self.db_path.display(),
            self.total_orphans,
            self.by_parent_table.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "total_orphans": self.total_orphans,
                "by_parent_table": self.by_parent_table,
                "sample": self.sample,
                "sample_cap": SAMPLE_CAP,
                "sample_truncated": self.total_orphans > SAMPLE_CAP,
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor archive-verify --json` to confirm the git archive is intact (orphan repair often co-occurs with a damaged archive).",
                        "If the archive is the source of truth: `am doctor reconstruct --dry-run --json` previews a fresh DB rebuild; `am doctor reconstruct --yes` applies it. The Op::Rename'd corrupt DB lands in the run-dir quarantine.",
                        "If the orphans are confined to a single parent table and the operator is confident: open a maintenance window, `sqlite3 <db_path>`, inspect with `PRAGMA foreign_key_check;`, then per-orphan: `INSERT INTO <table>_quarantine SELECT * FROM <table> WHERE rowid = ?;` followed by `DELETE FROM <table> WHERE rowid = ?;`. Re-run `PRAGMA foreign_key_check;` until empty.",
                        "Re-run `am doctor fix --only fm-db-state-files-orphan-foreign-key-rows --list` to confirm zero residual orphans.",
                    ],
                    "warning": "Auto-fix via Op::DbExec quarantine to `<table>_orphans` sibling tables is intentionally deferred in this first cut. Issuing arbitrary DELETE on orphan rows without a forensic dump destroys recovery evidence — use `am doctor reconstruct` instead, which preserves the corrupt DB byte-identically in the run-dir quarantine.",
                    "no_deletion_policy": "Per AGENTS.md RULE 1 the doctor never deletes. The future auto-fix will move orphans into sibling `<table>_orphans` tables, NOT delete them.",
                    "common_causes": [
                        "`am doctor reconstruct` pruned a parent row without cascading the child refs.",
                        "Manual `DELETE FROM agents` (or similar) issued with `PRAGMA foreign_keys = OFF`.",
                        "Partial restore where the parent table came from a fresher backup than the child table.",
                        "Schema migration that added a NOT NULL FK column with NULL fallbacks.",
                    ],
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

/// Detector. PURE w.r.t. caller-supplied DB paths.
///
/// Reads each DB read-only via URI `?immutable=1` (no -shm
/// creation, no locking). Runs `PRAGMA foreign_keys = ON` then
/// `PRAGMA foreign_key_check`. Returns one finding per DB that
/// has at least one orphan; healthy DBs are silently skipped.
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<OrphanForeignKeyRowsFinding> {
    let mut out = Vec::new();
    for db in candidate_dbs {
        if let Some(f) = detect_one(db) {
            out.push(f);
        }
    }
    out
}

fn detect_one(db_path: &Path) -> Option<OrphanForeignKeyRowsFinding> {
    if !db_path.exists() {
        return None;
    }
    let uri = format!("file:{}?immutable=1", db_path.to_string_lossy());
    let mut flags = OpenFlags::read_only();
    flags.uri = true;
    let config = SqliteConfig::file(uri).flags(flags);
    let conn = SqliteConnection::open(&config).ok()?;
    // foreign_keys must be ON for the check pragma to walk the
    // FK constraints. The pragma is per-connection, so this is
    // safe on a shared DB.
    conn.execute_raw("PRAGMA foreign_keys = ON").ok()?;
    let rows = conn.query_sync("PRAGMA foreign_key_check", &[]).ok()?;
    if rows.is_empty() {
        return None;
    }
    let mut by_parent_table: BTreeMap<String, usize> = BTreeMap::new();
    let mut sample: Vec<OrphanRow> = Vec::new();
    for row in &rows {
        // PRAGMA foreign_key_check columns are positional in
        // SQLite's pragma output: table, rowid, parent, fkid.
        // Some forks (frankensqlite) expose them as named
        // columns; we read both ways defensively.
        let child_table = row
            .get_named::<String>("table")
            .unwrap_or_else(|_| String::from("<unknown>"));
        let child_rowid = row.get_named::<i64>("rowid").unwrap_or(0);
        let parent_table = row
            .get_named::<String>("parent")
            .unwrap_or_else(|_| String::from("<unknown>"));
        let fkid = row.get_named::<i64>("fkid").unwrap_or(0);
        *by_parent_table.entry(parent_table.clone()).or_insert(0) += 1;
        if sample.len() < SAMPLE_CAP {
            sample.push(OrphanRow {
                child_table,
                child_rowid,
                parent_table,
                fkid,
            });
        }
    }
    Some(OrphanForeignKeyRowsFinding {
        db_path: db_path.to_path_buf(),
        total_orphans: rows.len(),
        by_parent_table,
        sample,
    })
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &OrphanForeignKeyRowsFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_healthy_db_with_fks(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // A small parent/child schema with a real FK constraint.
        // Both rows are present so foreign_key_check returns 0.
        conn.execute_raw(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parents(id));
             INSERT INTO parents (id, name) VALUES (1, 'p1');
             INSERT INTO children (id, parent_id) VALUES (10, 1);",
        )
        .unwrap();
        drop(conn);
        db
    }

    fn make_db_with_one_orphan(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // Build a parent + 2 children, then DELETE the parent
        // with FKs OFF — leaves the 2 children orphaned.
        conn.execute_raw(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY);
             CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parents(id));
             INSERT INTO parents (id) VALUES (1);
             INSERT INTO children (id, parent_id) VALUES (10, 1);
             INSERT INTO children (id, parent_id) VALUES (11, 1);
             PRAGMA foreign_keys = OFF;
             DELETE FROM parents WHERE id = 1;",
        )
        .unwrap();
        drop(conn);
        db
    }

    /// **NEGATIVE TEST FIRST**: a clean DB never flags.
    #[test]
    fn detector_returns_empty_for_healthy_db() {
        let td = TempDir::new().unwrap();
        let db = make_healthy_db_with_fks(&td);
        let findings = detect(std::slice::from_ref(&db));
        assert!(
            findings.is_empty(),
            "healthy DB must not flag: {findings:?}"
        );
    }

    /// **NEGATIVE**: empty input → no findings.
    #[test]
    fn detector_returns_empty_for_no_candidates() {
        assert!(detect(&[]).is_empty());
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_orphans_after_parent_delete_with_fks_off() {
        let td = TempDir::new().unwrap();
        let db = make_db_with_one_orphan(&td);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1, "must produce exactly one finding");
        let f = &findings[0];
        assert_eq!(f.total_orphans, 2, "two orphan children expected");
        assert_eq!(f.by_parent_table.get("parents"), Some(&2));
        assert_eq!(f.sample.len(), 2);
        for r in &f.sample {
            assert_eq!(r.child_table, "children");
            assert_eq!(r.parent_table, "parents");
        }
    }

    #[test]
    fn finding_sample_is_capped_at_sample_cap() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // Plant 25 orphans; sample must be capped at SAMPLE_CAP.
        let mut sql = String::from(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY);
             CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parents(id));
             INSERT INTO parents (id) VALUES (1);
             ",
        );
        for i in 0..25 {
            sql.push_str(&format!(
                "INSERT INTO children (id, parent_id) VALUES ({}, 1);\n",
                100 + i
            ));
        }
        sql.push_str("PRAGMA foreign_keys = OFF; DELETE FROM parents WHERE id = 1;");
        conn.execute_raw(&sql).unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.total_orphans, 25);
        assert_eq!(f.sample.len(), SAMPLE_CAP);
        assert_eq!(f.by_parent_table.get("parents"), Some(&25));
    }

    #[test]
    fn finding_serializes_with_sample_truncated_flag_and_remediation() {
        let f = OrphanForeignKeyRowsFinding {
            db_path: "/tmp/storage.sqlite3".into(),
            total_orphans: 50,
            by_parent_table: BTreeMap::from_iter([
                ("agents".to_string(), 30usize),
                ("projects".to_string(), 20usize),
            ]),
            sample: (0..10)
                .map(|i| OrphanRow {
                    child_table: format!("child_{}", i % 3),
                    child_rowid: i as i64,
                    parent_table: if i % 2 == 0 {
                        "agents".to_string()
                    } else {
                        "projects".to_string()
                    },
                    fkid: 0,
                })
                .collect(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"total_orphans\":50"));
        assert!(s.contains("by_parent_table"));
        assert!(s.contains("\"sample_truncated\":true"));
        assert!(s.contains("no_deletion_policy"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am doctor reconstruct"));
    }

    #[test]
    fn finding_sample_truncated_false_when_total_within_cap() {
        let f = OrphanForeignKeyRowsFinding {
            db_path: "/tmp/storage.sqlite3".into(),
            total_orphans: 3,
            by_parent_table: BTreeMap::from_iter([("agents".to_string(), 3usize)]),
            sample: (0..3)
                .map(|i| OrphanRow {
                    child_table: "x".to_string(),
                    child_rowid: i as i64,
                    parent_table: "agents".to_string(),
                    fkid: 0,
                })
                .collect(),
        };
        let s = serde_json::to_string(&f.to_finding()).unwrap();
        assert!(s.contains("\"sample_truncated\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
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
        let finding = OrphanForeignKeyRowsFinding {
            db_path: "/tmp/x".into(),
            total_orphans: 0,
            by_parent_table: BTreeMap::new(),
            sample: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
