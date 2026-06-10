//! `fm-db-state-files-orphan-foreign-key-rows` — P1 partial auto-fix.
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
//! Auto-fix is intentionally narrow: stale `file_reservations`
//! rows whose holder agent or project row is gone, plus orphaned
//! `file_reservation_releases` rows whose reservation row is gone,
//! are moved into `doctor_orphan_file_reservations` /
//! `doctor_orphan_file_reservation_releases` and then removed from
//! the live reservation tables. This mirrors the v23 migration
//! cleanup while preserving forensic rows in the same SQLite
//! database and relying on the doctor `Op::DbExec` chokepoint for
//! whole-file backup/undo.
//!
//! Message-recipient rows whose agent is missing remain detect-only
//! by design because they preserve mailbox history as
//! unknown-recipient metadata.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use serde::Serialize;
use sqlmodel_core::Row;
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
    /// Per-child-table counts (e.g., `file_reservations -> 1044`).
    /// This drives the auto-fixability decision without relying on
    /// the bounded sample.
    pub by_child_table: BTreeMap<String, usize>,
    /// Per-parent-table counts (e.g., `agents → 3, projects → 1`).
    /// Useful for operators deciding which recovery path applies.
    pub by_parent_table: BTreeMap<String, usize>,
    /// Bounded sample (≤ SAMPLE_CAP rows) for operator triage.
    pub sample: Vec<OrphanRow>,
}

impl OrphanForeignKeyRowsFinding {
    pub fn to_finding(&self) -> super::Finding {
        let auto_fixable = self.has_auto_fixable_rows();
        let command = if auto_fixable {
            format!("am doctor fix --only {FM_ID} --yes")
        } else {
            format!("am doctor explain {FM_ID}")
        };
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
                "by_child_table": self.by_child_table,
                "by_parent_table": self.by_parent_table,
                "sample": self.sample,
                "sample_cap": SAMPLE_CAP,
                "sample_truncated": self.total_orphans > SAMPLE_CAP,
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor archive-verify --json` to confirm the git archive is intact (orphan repair often co-occurs with a damaged archive).",
                        "For stale file reservation rows whose holder agent/project is gone, or stale file reservation release-ledger rows whose reservation row is gone: `am doctor fix --only fm-db-state-files-orphan-foreign-key-rows --dry-run` previews the reversible DbExec quarantine; `--yes` applies it.",
                        "If the archive is the source of truth for a broader DB rebuild: `am doctor reconstruct --dry-run --json` previews a fresh DB rebuild; `am doctor reconstruct --yes` applies it. The Op::Rename'd corrupt DB lands in the run-dir quarantine.",
                        "If the orphans are confined to a single parent table and the operator is confident: open a maintenance window, `sqlite3 <db_path>`, inspect with `PRAGMA foreign_key_check;`, then per-orphan: `INSERT INTO <table>_quarantine SELECT * FROM <table> WHERE rowid = ?;` followed by `DELETE FROM <table> WHERE rowid = ?;`. Re-run `PRAGMA foreign_key_check;` until empty.",
                        "Re-run `am doctor fix --only fm-db-state-files-orphan-foreign-key-rows --list` to confirm zero residual orphans.",
                    ],
                    "warning": "Auto-fix is deliberately limited to stale file_reservations and file_reservation_releases rows. Message-recipient rows with missing agent metadata remain preserved by default because they carry mailbox history.",
                    "no_deletion_policy": "Per AGENTS.md RULE 1 the doctor never deletes files. The DbExec fix moves reservation orphans into doctor_orphan_* tables before removing them from live reservation tables.",
                    "common_causes": [
                        "`am doctor reconstruct` pruned a parent row without cascading the child refs.",
                        "Manual `DELETE FROM agents` (or similar) issued with `PRAGMA foreign_keys = OFF`.",
                        "Partial restore where the parent table came from a fresher backup than the child table.",
                        "Schema migration that added a NOT NULL FK column with NULL fallbacks.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command,
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable,
                estimated_actions: usize::from(auto_fixable),
            },
        }
    }

    fn has_auto_fixable_rows(&self) -> bool {
        self.by_child_table.contains_key("file_reservations")
            || self
                .by_child_table
                .contains_key("file_reservation_releases")
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
    let conn = super::open_immutable_sqlite(db_path).ok()?;
    // foreign_keys must be ON for the check pragma to walk the
    // FK constraints. The pragma is per-connection, so this is
    // safe on a shared DB.
    conn.execute_raw("PRAGMA foreign_keys = ON").ok()?;
    let rows = conn.query_sync("PRAGMA foreign_key_check", &[]).ok()?;
    if rows.is_empty() {
        return None;
    }
    let mut by_child_table: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_parent_table: BTreeMap<String, usize> = BTreeMap::new();
    let mut sample: Vec<OrphanRow> = Vec::new();
    for row in &rows {
        // PRAGMA foreign_key_check columns are positional in
        // SQLite's pragma output: table, rowid, parent, fkid.
        // Some forks (frankensqlite) expose them as named
        // columns; we read both ways defensively.
        let orphan = orphan_row_from_foreign_key_check(row);
        let child_table = orphan.child_table.clone();
        let parent_table = orphan.parent_table.clone();
        *by_child_table.entry(child_table.clone()).or_insert(0) += 1;
        *by_parent_table.entry(parent_table.clone()).or_insert(0) += 1;
        if sample.len() < SAMPLE_CAP {
            sample.push(orphan);
        }
    }
    Some(OrphanForeignKeyRowsFinding {
        db_path: db_path.to_path_buf(),
        total_orphans: rows.len(),
        by_child_table,
        by_parent_table,
        sample,
    })
}

fn orphan_row_from_foreign_key_check(row: &Row) -> OrphanRow {
    OrphanRow {
        child_table: fk_string_column(row, "table", 0),
        child_rowid: fk_i64_column(row, "rowid", 1),
        parent_table: fk_string_column(row, "parent", 2),
        fkid: fk_i64_column(row, "fkid", 3),
    }
}

fn fk_string_column(row: &Row, name: &str, index: usize) -> String {
    match row.get_named::<String>(name) {
        Ok(value) => value,
        Err(_) => row
            .get_as::<String>(index)
            .unwrap_or_else(|_| String::from("<unknown>")),
    }
}

fn fk_i64_column(row: &Row, name: &str, index: usize) -> i64 {
    match row.get_named::<i64>(name) {
        Ok(value) => value,
        Err(_) => row.get_as::<i64>(index).unwrap_or(0),
    }
}

pub fn fix(
    ctx: &MutateContext,
    finding: &OrphanForeignKeyRowsFinding,
) -> Result<FixOutcome, MutateError> {
    if !finding.has_auto_fixable_rows() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    let result = mutate(
        ctx,
        &finding.db_path,
        Op::DbExec {
            sql: orphan_file_reservations_quarantine_sql(),
        },
    )?;
    let residual_auto_fixable = if ctx.dry_run || !result.ok {
        false
    } else {
        detect(std::slice::from_ref(&finding.db_path))
            .iter()
            .any(OrphanForeignKeyRowsFinding::has_auto_fixable_rows)
    };
    Ok(FixOutcome {
        actions_taken: usize::from(result.ok),
        actions_skipped: usize::from(!result.ok) + usize::from(residual_auto_fixable),
        quarantined_paths: Vec::new(),
    })
}

fn orphan_file_reservations_quarantine_sql() -> String {
    r"
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS doctor_orphan_file_reservations (
    id INTEGER PRIMARY KEY,
    project_id INTEGER,
    agent_id INTEGER,
    path_pattern TEXT,
    exclusive INTEGER,
    reason TEXT,
    created_ts INTEGER,
    expires_ts INTEGER,
    released_ts INTEGER,
    quarantined_at_ts INTEGER NOT NULL,
    quarantine_reason TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS doctor_orphan_file_reservation_releases (
    reservation_id INTEGER PRIMARY KEY,
    released_ts INTEGER,
    quarantined_at_ts INTEGER NOT NULL,
    quarantine_reason TEXT NOT NULL
);

INSERT OR IGNORE INTO doctor_orphan_file_reservations (
    id,
    project_id,
    agent_id,
    path_pattern,
    exclusive,
    reason,
    created_ts,
    expires_ts,
    released_ts,
    quarantined_at_ts,
    quarantine_reason
)
SELECT
    id,
    project_id,
    agent_id,
    path_pattern,
    exclusive,
    reason,
    created_ts,
    expires_ts,
    released_ts,
    CAST(strftime('%s', 'now') AS INTEGER) * 1000000,
    'foreign_key_check: missing file_reservations agent/project parent'
FROM file_reservations
WHERE (agent_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM agents WHERE agents.id = file_reservations.agent_id))
   OR (project_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM projects WHERE projects.id = file_reservations.project_id));

INSERT OR IGNORE INTO doctor_orphan_file_reservation_releases (
    reservation_id,
    released_ts,
    quarantined_at_ts,
    quarantine_reason
)
SELECT
    r.reservation_id,
    r.released_ts,
    CAST(strftime('%s', 'now') AS INTEGER) * 1000000,
    'foreign_key_check: missing file_reservations row'
FROM file_reservation_releases AS r
WHERE (r.reservation_id IS NOT NULL
       AND NOT EXISTS (
       SELECT 1 FROM file_reservations
       WHERE file_reservations.id = r.reservation_id
   ))
   OR r.reservation_id IN (
       SELECT id FROM file_reservations
       WHERE (agent_id IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM agents WHERE agents.id = file_reservations.agent_id))
          OR (project_id IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM projects WHERE projects.id = file_reservations.project_id))
   );

DELETE FROM file_reservations
WHERE ((agent_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM agents WHERE agents.id = file_reservations.agent_id))
   OR (project_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM projects WHERE projects.id = file_reservations.project_id)))
  AND EXISTS (
      SELECT 1 FROM doctor_orphan_file_reservations AS q
      WHERE q.id = file_reservations.id
        AND q.project_id IS file_reservations.project_id
        AND q.agent_id IS file_reservations.agent_id
        AND q.path_pattern IS file_reservations.path_pattern
        AND q.exclusive IS file_reservations.exclusive
        AND q.reason IS file_reservations.reason
        AND q.created_ts IS file_reservations.created_ts
        AND q.expires_ts IS file_reservations.expires_ts
        AND q.released_ts IS file_reservations.released_ts
  );

DELETE FROM file_reservation_releases
WHERE reservation_id IS NOT NULL
  AND NOT EXISTS (
       SELECT 1 FROM file_reservations
       WHERE file_reservations.id = file_reservation_releases.reservation_id
  )
  AND EXISTS (
      SELECT 1 FROM doctor_orphan_file_reservation_releases AS q
      WHERE q.reservation_id = file_reservation_releases.reservation_id
        AND q.released_ts IS file_reservation_releases.released_ts
  );

COMMIT;
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlmodel_sqlite::SqliteConnection;
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

    #[cfg(unix)]
    #[test]
    fn detector_handles_uri_metacharacters_in_db_path() {
        let td = TempDir::new().unwrap();
        let weird_dir = td.path().join("agent mail ?#%");
        std::fs::create_dir(&weird_dir).unwrap();
        let db = weird_dir.join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY);
             CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parents(id));
             INSERT INTO parents (id) VALUES (1);
             INSERT INTO children (id, parent_id) VALUES (10, 1);
             PRAGMA foreign_keys = OFF;
             DELETE FROM parents WHERE id = 1;",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(
            findings.len(),
            1,
            "URI metacharacters in the DB path must not hide FK findings"
        );
        assert_eq!(findings[0].total_orphans, 1);
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
            by_child_table: BTreeMap::from_iter([
                ("child_0".to_string(), 17usize),
                ("child_1".to_string(), 17usize),
                ("child_2".to_string(), 16usize),
            ]),
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
        assert!(s.contains("by_child_table"));
        assert!(s.contains("by_parent_table"));
        assert!(s.contains("\"sample_truncated\":true"));
        assert!(s.contains("no_deletion_policy"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert_eq!(g.remediation.command, format!("am doctor explain {FM_ID}"));
        assert!(s.contains("am doctor reconstruct"));
    }

    #[test]
    fn finding_uses_fix_command_for_reservation_orphans() {
        let f = OrphanForeignKeyRowsFinding {
            db_path: "/tmp/storage.sqlite3".into(),
            total_orphans: 2,
            by_child_table: BTreeMap::from_iter([
                ("file_reservations".to_string(), 1usize),
                ("file_reservation_releases".to_string(), 1usize),
            ]),
            by_parent_table: BTreeMap::from_iter([
                ("agents".to_string(), 1usize),
                ("file_reservations".to_string(), 1usize),
            ]),
            sample: Vec::new(),
        };
        let g = f.to_finding();
        assert!(g.remediation.auto_fixable);
        assert_eq!(
            g.remediation.command,
            format!("am doctor fix --only {FM_ID} --yes")
        );
        assert_eq!(g.remediation.estimated_actions, 1);
    }

    #[test]
    fn foreign_key_check_reader_falls_back_to_positional_columns() {
        let row = sqlmodel_core::Row::new(
            vec![
                "col0".to_string(),
                "col1".to_string(),
                "col2".to_string(),
                "col3".to_string(),
            ],
            vec![
                sqlmodel_core::Value::Text("file_reservation_releases".to_string()),
                sqlmodel_core::Value::Int(99),
                sqlmodel_core::Value::Text("file_reservations".to_string()),
                sqlmodel_core::Value::Int(0),
            ],
        );
        let orphan = orphan_row_from_foreign_key_check(&row);
        assert_eq!(orphan.child_table, "file_reservation_releases");
        assert_eq!(orphan.child_rowid, 99);
        assert_eq!(orphan.parent_table, "file_reservations");
        assert_eq!(orphan.fkid, 0);
    }

    #[test]
    fn finding_sample_truncated_false_when_total_within_cap() {
        let f = OrphanForeignKeyRowsFinding {
            db_path: "/tmp/storage.sqlite3".into(),
            total_orphans: 3,
            by_child_table: BTreeMap::from_iter([("x".to_string(), 3usize)]),
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
    fn fixer_skips_non_reservation_orphans() {
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
            by_child_table: BTreeMap::new(),
            by_parent_table: BTreeMap::new(),
            sample: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn fixer_quarantines_and_removes_orphan_file_reservations() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 agent_id INTEGER NOT NULL REFERENCES agents(id),
                 path_pattern TEXT NOT NULL,
                 exclusive INTEGER NOT NULL DEFAULT 1,
                 reason TEXT NOT NULL DEFAULT '',
                 created_ts INTEGER NOT NULL,
                 expires_ts INTEGER NOT NULL,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservations
                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                 (10, 1, 1, 'src/**', 1, 'valid', 100, 200, NULL),
                 (11, 1, 404, 'old/**', 1, 'orphan', 100, 200, 150);
             INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (11, 150);",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
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

        let outcome = fix(&ctx, &findings[0]).expect("fix reservation orphans");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let fk_rows = verify.query_sync("PRAGMA foreign_key_check", &[]).unwrap();
        assert!(fk_rows.is_empty(), "remaining FK rows: {fk_rows:?}");
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 11",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(live_count, 0);
        let quarantined = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservations WHERE id = 11",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(quarantined, 1);
        let quarantined_release = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservation_releases WHERE reservation_id = 11",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(quarantined_release, 1);
    }

    #[test]
    fn fixer_quarantines_file_reservations_with_missing_project() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 agent_id INTEGER NOT NULL REFERENCES agents(id),
                 path_pattern TEXT NOT NULL,
                 exclusive INTEGER NOT NULL DEFAULT 1,
                 reason TEXT NOT NULL DEFAULT '',
                 created_ts INTEGER NOT NULL,
                 expires_ts INTEGER NOT NULL,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservations
                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                 (12, 404, 1, 'missing-project/**', 1, 'orphan-project', 100, 200, NULL);",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].by_parent_table.get("projects"), Some(&1));
        assert!(findings[0].has_auto_fixable_rows());

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

        let outcome = fix(&ctx, &findings[0]).expect("fix missing-project reservation");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let fk_rows = verify.query_sync("PRAGMA foreign_key_check", &[]).unwrap();
        assert!(fk_rows.is_empty(), "remaining FK rows: {fk_rows:?}");
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 12",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(live_count, 0);
        let quarantined = verify
            .query_sync(
                "SELECT quarantine_reason FROM doctor_orphan_file_reservations WHERE id = 12",
                &[],
            )
            .unwrap();
        assert_eq!(quarantined.len(), 1);
        assert!(
            quarantined[0]
                .get_named::<String>("quarantine_reason")
                .unwrap()
                .contains("agent/project parent")
        );
    }

    #[test]
    fn fixer_ignores_null_child_keys_that_foreign_key_check_allows() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER REFERENCES projects(id),
                 agent_id INTEGER REFERENCES agents(id),
                 path_pattern TEXT,
                 exclusive INTEGER,
                 reason TEXT,
                 created_ts INTEGER,
                 expires_ts INTEGER,
                 released_ts INTEGER
             );
	             CREATE TABLE file_reservation_releases (
	                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
	                 released_ts INTEGER
	             );
	             INSERT INTO projects (id) VALUES (1);
	             INSERT INTO agents (id, project_id) VALUES (1, 1);
	             INSERT INTO file_reservations
	                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                 (12, NULL, 1, NULL, NULL, NULL, NULL, NULL, NULL),
                 (13, 1, 404, 'old/**', 1, 'orphan', 100, 200, NULL);",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);

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

        let outcome =
            fix(&ctx, &findings[0]).expect("fix nullable child-key and missing-parent rows");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let fk_rows = verify.query_sync("PRAGMA foreign_key_check", &[]).unwrap();
        assert!(fk_rows.is_empty(), "remaining FK rows: {fk_rows:?}");
        let nullable_child_key_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 12",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(
            nullable_child_key_count, 1,
            "NULL child keys are allowed by SQLite FK semantics and must not be quarantined by this FM"
        );
        let orphan_live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 13",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(orphan_live_count, 0);
        let quarantined_orphan = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservations WHERE id = 13",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(quarantined_orphan, 1);
    }

    #[test]
    fn fixer_reports_residual_when_orphan_cannot_be_quarantined() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER REFERENCES projects(id),
                 agent_id INTEGER REFERENCES agents(id),
                 path_pattern TEXT,
                 exclusive INTEGER,
                 reason TEXT,
                 created_ts INTEGER,
                 expires_ts INTEGER,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER
             );
             CREATE TABLE doctor_orphan_file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL,
                 agent_id INTEGER NOT NULL,
                 path_pattern TEXT NOT NULL,
                 exclusive INTEGER NOT NULL,
                 reason TEXT NOT NULL,
                 created_ts INTEGER NOT NULL,
                 expires_ts INTEGER NOT NULL,
                 released_ts INTEGER,
                 quarantined_at_ts INTEGER NOT NULL,
                 quarantine_reason TEXT NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservations
                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                 (14, 1, 404, NULL, 1, 'orphan-with-null-copy-field', 100, 200, NULL);",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].has_auto_fixable_rows());

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

        let outcome =
            fix(&ctx, &findings[0]).expect("fix with preexisting strict quarantine table");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 1);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 14",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(live_count, 1);
        let quarantined_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservations WHERE id = 14",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(quarantined_count, 0);
        let remaining = detect(std::slice::from_ref(&db));
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].has_auto_fixable_rows());
    }

    #[test]
    fn fixer_does_not_delete_reservation_when_existing_quarantine_copy_differs() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER REFERENCES projects(id),
                 agent_id INTEGER REFERENCES agents(id),
                 path_pattern TEXT,
                 exclusive INTEGER,
                 reason TEXT,
                 created_ts INTEGER,
                 expires_ts INTEGER,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER
             );
             CREATE TABLE doctor_orphan_file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER,
                 agent_id INTEGER,
                 path_pattern TEXT,
                 exclusive INTEGER,
                 reason TEXT,
                 created_ts INTEGER,
                 expires_ts INTEGER,
                 released_ts INTEGER,
                 quarantined_at_ts INTEGER NOT NULL,
                 quarantine_reason TEXT NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservations
                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts)
             VALUES
                 (15, 1, 404, 'current/**', 1, 'current orphan', 100, 200, NULL);
             INSERT INTO doctor_orphan_file_reservations
                 (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts, quarantined_at_ts, quarantine_reason)
             VALUES
                 (15, 1, 404, 'stale/**', 1, 'stale copy', 10, 20, NULL, 30, 'old quarantine');",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].has_auto_fixable_rows());

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

        let outcome = fix(&ctx, &findings[0]).expect("fix with stale quarantine collision");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 1);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservations WHERE id = 15",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(
            live_count, 1,
            "live row must remain when the existing quarantine copy differs"
        );
        let stale_copy_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservations WHERE id = 15 AND path_pattern = 'stale/**'",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(stale_copy_count, 1);
    }

    #[test]
    fn fixer_quarantines_release_sidecar_orphans_without_reservation_row() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 agent_id INTEGER NOT NULL REFERENCES agents(id),
                 path_pattern TEXT NOT NULL,
                 exclusive INTEGER NOT NULL DEFAULT 1,
                 reason TEXT NOT NULL DEFAULT '',
                 created_ts INTEGER NOT NULL,
                 expires_ts INTEGER NOT NULL,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (99, 150);",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].by_child_table.get("file_reservation_releases"),
            Some(&1)
        );
        assert!(findings[0].has_auto_fixable_rows());

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

        let outcome = fix(&ctx, &findings[0]).expect("fix release-ledger orphan");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let fk_rows = verify.query_sync("PRAGMA foreign_key_check", &[]).unwrap();
        assert!(fk_rows.is_empty(), "remaining FK rows: {fk_rows:?}");
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservation_releases WHERE reservation_id = 99",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(live_count, 0);
        let quarantined_release = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservation_releases WHERE reservation_id = 99",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(quarantined_release, 1);
    }

    #[test]
    fn fixer_does_not_delete_release_when_existing_quarantine_copy_differs() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE projects (id INTEGER PRIMARY KEY);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id));
             CREATE TABLE file_reservations (
                 id INTEGER PRIMARY KEY,
                 project_id INTEGER NOT NULL REFERENCES projects(id),
                 agent_id INTEGER NOT NULL REFERENCES agents(id),
                 path_pattern TEXT NOT NULL,
                 exclusive INTEGER NOT NULL DEFAULT 1,
                 reason TEXT NOT NULL DEFAULT '',
                 created_ts INTEGER NOT NULL,
                 expires_ts INTEGER NOT NULL,
                 released_ts INTEGER
             );
             CREATE TABLE file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY REFERENCES file_reservations(id),
                 released_ts INTEGER NOT NULL
             );
             CREATE TABLE doctor_orphan_file_reservation_releases (
                 reservation_id INTEGER PRIMARY KEY,
                 released_ts INTEGER,
                 quarantined_at_ts INTEGER NOT NULL,
                 quarantine_reason TEXT NOT NULL
             );
             INSERT INTO projects (id) VALUES (1);
             INSERT INTO agents (id, project_id) VALUES (1, 1);
             INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (99, 150);
             INSERT INTO doctor_orphan_file_reservation_releases
                 (reservation_id, released_ts, quarantined_at_ts, quarantine_reason)
             VALUES
                 (99, 149, 30, 'old quarantine');",
        )
        .unwrap();
        drop(conn);

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].has_auto_fixable_rows());

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

        let outcome = fix(&ctx, &findings[0]).expect("fix with stale release quarantine collision");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 1);

        let verify = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let live_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM file_reservation_releases WHERE reservation_id = 99",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(
            live_count, 1,
            "live release row must remain when the existing quarantine copy differs"
        );
        let stale_copy_count = verify
            .query_sync(
                "SELECT COUNT(*) AS count FROM doctor_orphan_file_reservation_releases WHERE reservation_id = 99 AND released_ts = 149",
                &[],
            )
            .unwrap()[0]
            .get_named::<i64>("count")
            .unwrap();
        assert_eq!(stale_copy_count, 1);
    }
}
