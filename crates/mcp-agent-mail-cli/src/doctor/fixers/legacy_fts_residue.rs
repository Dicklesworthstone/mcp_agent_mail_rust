//! `fm-db-state-files-legacy-fts-residue` — P2.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! Search V3 (frankensearch-managed) replaced the previous FTS5
//! backend, but the SQLite database may still carry the legacy
//! `fts_messages` / `fts_agents` / `fts_projects` tables plus
//! their `*_ai`/`*_ad`/`*_au` triggers. These artifacts are
//! harmless on disk but waste space and confuse anyone reading
//! `sqlite_master` directly — and a future schema migration
//! that re-uses the `fts_*` namespace would conflict.
//!
//! ## Detection (pure, on-disk state)
//!
//! 1. Open `storage.sqlite3` read-only.
//! 2. Verify Search V3 is active by probing the canonical marker
//!    file: `<storage_root>/search_index/.managed.json`. If
//!    absent, FTS5 IS the active backend and residue is normal —
//!    no finding.
//! 3. Query `sqlite_master` for `name LIKE 'fts_%' AND type IN
//!    ('table', 'trigger', 'view')`. Any rows → emit finding
//!    enumerating them.
//!
//! Unlike `busy_timeout` (pass-35V), `sqlite_master` IS persistent
//! on-disk state, so a fresh connection sees the same residue
//! that the pool's connections would see. The detection
//! mechanism is sound.
//!
//! ## Fix
//!
//! **Detect-only in this first cut.** The repair_spec's full
//! Op::DbExec drop sequence (TRIGGER → VIEW → TABLE) is correct
//! per AGENTS.md's frankensqlite notes ("DROP TRIGGER is fully
//! functional"), but the dependency-ordered drop loop plus
//! `sqlite3 .dump`-style backup of the master rows requires
//! more chokepoint plumbing than fits this commit. Manual
//! remediation envelope points operators at running the
//! documented sequence by hand.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-legacy-fts-residue";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Sentinel filename that marks the `<storage_root>/search_index/`
/// directory as managed by Search V3. Created by the frankensearch
/// IndexBuilder on first activation; existence is the V3-active
/// signal.
pub const V3_MANAGED_MARKER: &str = ".managed.json";

#[derive(Debug, Clone, Serialize)]
pub struct LegacyFtsResidueFinding {
    pub db_path: PathBuf,
    /// Path to the Search V3 marker that establishes "V3 is active".
    pub v3_marker_path: PathBuf,
    /// One entry per residual sqlite_master row.
    pub residual_objects: Vec<ResidualObject>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidualObject {
    /// `"table"`, `"trigger"`, or `"view"`.
    pub kind: String,
    /// The `name` column from `sqlite_master`.
    pub name: String,
}

impl LegacyFtsResidueFinding {
    pub fn to_finding(&self) -> super::Finding {
        let count = self.residual_objects.len();
        let title = format!(
            "DB {} retains {} legacy FTS5 object(s) after Search V3 migration (manual DROP sequence required)",
            self.db_path.display(),
            count,
        );
        let object_summary: Vec<String> = self
            .residual_objects
            .iter()
            .map(|o| format!("{} {}", o.kind, o.name))
            .collect();
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "v3_marker_path": self.v3_marker_path.to_string_lossy(),
                "residual_count": count,
                "residual_objects": self.residual_objects,
                "manual_remediation": {
                    "steps": [
                        "Take a `sqlite3 storage.sqlite3 .dump` backup of the fts_% rows BEFORE dropping them (so an undo is possible).",
                        "Drop triggers first (DROP TRIGGER IF EXISTS <name>;), then views (DROP VIEW IF EXISTS <name>;), then virtual tables (DROP TABLE IF EXISTS <name>;) — order matters for FK / dependency reasons.",
                        "Re-run `am doctor --only fm-db-state-files-legacy-fts-residue` to confirm sqlite_master is clean.",
                    ],
                    "note": "Auto-fix via Op::DbExec is intentionally deferred in this first cut — the dependency-ordered drop sequence + backup of dropped rows needs additional chokepoint plumbing.",
                    "residue_summary": object_summary,
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
/// `candidate_paths` is typically `[<storage_root>/storage.sqlite3]`.
/// Empty slice skips the FM. Detector skips silently when:
/// - the DB path is not a regular file
/// - the V3 marker file (`<db_parent>/search_index/.managed.json`)
///   doesn't exist (FTS5 is the active backend; residue is normal)
/// - the DB can't be opened
/// - the sqlite_master query fails
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<LegacyFtsResidueFinding> {
    use sqlmodel_sqlite::SqliteConnection;
    let mut out = Vec::new();
    for db_path in candidate_paths {
        if !db_path.is_file() {
            continue;
        }
        let Some(parent) = db_path.parent() else {
            continue;
        };
        let marker_path = parent.join("search_index").join(V3_MANAGED_MARKER);
        if !marker_path.exists() {
            // FTS5 is the active backend; fts_* objects are
            // expected.
            continue;
        }
        let Ok(conn) = SqliteConnection::open_file(db_path.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(residue) = read_fts_residue(&conn) else {
            continue;
        };
        if residue.is_empty() {
            continue;
        }
        out.push(LegacyFtsResidueFinding {
            db_path: db_path.clone(),
            v3_marker_path: marker_path,
            residual_objects: residue,
        });
    }
    out
}

/// Read all `sqlite_master` rows whose `name` matches the legacy
/// FTS5 prefix. Returns `Some(rows)` on success (possibly empty),
/// `None` on query error.
fn read_fts_residue(conn: &sqlmodel_sqlite::SqliteConnection) -> Option<Vec<ResidualObject>> {
    let rows = conn
        .query_sync(
            "SELECT type, name FROM sqlite_master \
             WHERE name LIKE 'fts_%' AND type IN ('table', 'trigger', 'view')",
            &[],
        )
        .ok()?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let kind = r.get_named::<String>("type").ok()?;
        let name = r.get_named::<String>("name").ok()?;
        out.push(ResidualObject { kind, name });
    }
    Some(out)
}

/// Fixer. Detect-only — returns `actions_skipped: 1`. The full
/// repair_spec Op::DbExec drop sequence is deferred.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &LegacyFtsResidueFinding,
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

    /// Helper: create a SQLite DB with optional FTS5 table + trigger
    /// to simulate legacy residue. Optionally also create the V3
    /// marker file.
    fn setup_db(
        td: &TempDir,
        create_fts_residue: bool,
        create_v3_marker: bool,
    ) -> (PathBuf, PathBuf) {
        let db_path = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db_path.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);")
            .expect("create main table");
        if create_fts_residue {
            // FTS5 virtual table — the canonical residue shape.
            // We use a regular table named `fts_messages` rather
            // than a true FTS5 virtual table because in-process
            // SQLite test builds may not have FTS5 compiled in,
            // and the detector's filter is purely by `name LIKE
            // 'fts_%'` so the type-of-virtual-vs-regular doesn't
            // affect detection.
            conn.execute_raw("CREATE TABLE fts_messages (rowid INTEGER, content TEXT);")
                .expect("create fts_messages table");
            // A trigger named with the canonical FTS5 suffix.
            conn.execute_raw(
                "CREATE TRIGGER fts_messages_ai AFTER INSERT ON messages BEGIN \
                 INSERT INTO fts_messages(rowid, content) VALUES (NEW.id, NEW.body); \
                 END;",
            )
            .expect("create fts trigger");
        }
        drop(conn);

        let marker_path = td.path().join("search_index").join(V3_MANAGED_MARKER);
        if create_v3_marker {
            fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
            fs::write(&marker_path, r#"{"version":1,"frankensearch_managed":true}"#).unwrap();
        }
        (db_path, marker_path)
    }

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): a properly-clean
    /// DB with V3 active must NOT produce a finding. Without this
    /// test, an always-flagging detector would still pass.
    #[test]
    fn detector_skips_clean_db_with_v3_active() {
        let td = TempDir::new().unwrap();
        let (db, _marker) = setup_db(&td, /*residue=*/ false, /*v3_marker=*/ true);
        let findings = detect(&[db]);
        assert!(
            findings.is_empty(),
            "V3-active DB without fts_% residue must NOT be flagged; got {} finding(s)",
            findings.len()
        );
    }

    /// **NEGATIVE TEST**: when Search V3 is NOT active (no marker
    /// file), fts_% residue is expected/normal — detector skips.
    #[test]
    fn detector_skips_when_v3_marker_absent() {
        let td = TempDir::new().unwrap();
        let (db, _marker) = setup_db(&td, /*residue=*/ true, /*v3_marker=*/ false);
        let findings = detect(&[db]);
        assert!(
            findings.is_empty(),
            "without V3 marker, fts_* residue is normal (FTS5 is the active backend)"
        );
    }

    /// **POSITIVE**: V3 active + residue present → flag.
    #[test]
    fn detector_flags_v3_active_db_with_fts_residue() {
        let td = TempDir::new().unwrap();
        let (db, marker) = setup_db(&td, /*residue=*/ true, /*v3_marker=*/ true);
        let findings = detect(&[db.clone()]);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.db_path, db);
        assert_eq!(f.v3_marker_path, marker);
        assert_eq!(
            f.residual_objects.len(),
            2,
            "expected fts_messages table + fts_messages_ai trigger"
        );
        let kinds: Vec<&str> = f.residual_objects.iter().map(|o| o.kind.as_str()).collect();
        assert!(kinds.contains(&"table"));
        assert!(kinds.contains(&"trigger"));
        let names: Vec<&str> = f.residual_objects.iter().map(|o| o.name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("fts_messages")));
    }

    #[test]
    fn detector_skips_missing_path() {
        let findings = detect(&[PathBuf::from("/nonexistent/storage.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_empty_input() {
        let findings = detect(&[]);
        assert!(findings.is_empty());
    }

    #[test]
    fn finding_serializes_with_residue_inventory_and_manual_remediation() {
        let f = LegacyFtsResidueFinding {
            db_path: "/var/data/storage.sqlite3".into(),
            v3_marker_path: "/var/data/search_index/.managed.json".into(),
            residual_objects: vec![
                ResidualObject {
                    kind: "table".into(),
                    name: "fts_messages".into(),
                },
                ResidualObject {
                    kind: "trigger".into(),
                    name: "fts_messages_ai".into(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"residual_count\":2"));
        assert!(s.contains("fts_messages"));
        assert!(s.contains("manual_remediation"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("DROP TRIGGER"));
    }

    #[test]
    fn finding_title_includes_residue_count() {
        let f = LegacyFtsResidueFinding {
            db_path: "/var/data/storage.sqlite3".into(),
            v3_marker_path: "/var/data/search_index/.managed.json".into(),
            residual_objects: vec![ResidualObject {
                kind: "table".into(),
                name: "fts_messages".into(),
            }],
        };
        let g = f.to_finding();
        assert!(g.title.contains("retains 1 legacy FTS5 object"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
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
        let finding = LegacyFtsResidueFinding {
            db_path: td.path().join("storage.sqlite3"),
            v3_marker_path: td.path().join("search_index").join(V3_MANAGED_MARKER),
            residual_objects: vec![ResidualObject {
                kind: "table".into(),
                name: "fts_messages".into(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(outcome.quarantined_paths.is_empty());
    }
}
