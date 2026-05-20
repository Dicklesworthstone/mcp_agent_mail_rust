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
//! **Auto-fix via `Op::DbExec`.** The residual objects are
//! dropped in dependency order — TRIGGER first, then VIEW, then
//! TABLE — as a single `Op::DbExec` carrying the ordered
//! `DROP ... IF EXISTS` sequence. The chokepoint byte-copies the
//! whole DB file to its backup BEFORE executing, so
//! `am doctor undo <run-id>` restores the pre-fix DB
//! byte-identically (the DROP is reversed wholesale by the
//! file-level restore — no row-level replay needed).
//!
//! Each `DROP ... IF EXISTS` is individually idempotent: running
//! the fix twice (or on an already-clean DB) drops nothing on the
//! second pass.
//!
//! WAL caveat: the chokepoint backs up the main DB file but not
//! `-wal` / `-shm` sidecars. For the pooled production DB an
//! operator should `PRAGMA wal_checkpoint(TRUNCATE)` and quiesce
//! writers before running `am doctor fix`; the doctor's premise
//! is operator-supervised remediation, not live mutation. The DB
//! created by tests uses the default delete-journal mode, so no
//! sidecars persist past the drop.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-legacy-fts-residue";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Sentinel filename that marks the `<storage_root>/search_index/`
/// directory as managed by Search V3. Created by the frankensearch
/// IndexBuilder on first activation; existence is the V3-active
/// signal.
///
/// **Known limitation** (pass-35Y review F1): the marker location
/// (`<storage_root>/search_index/.managed.json`) is an internal
/// implementation detail of the external `frankensearch` crate.
/// If frankensearch ever refactors the marker name or relative
/// path, this detector silently downgrades to a no-op. A future
/// hardening pass should swap this for
/// `frankensearch::is_v3_managed(path)` once frankensearch
/// exports that helper.
pub const V3_MANAGED_MARKER: &str = ".managed.json";

/// Canonical legacy FTS5 table-name prefixes. The detector emits a
/// finding only when sqlite_master rows match one of these
/// prefixes (plus optional FTS5 shadow-table or trigger
/// suffixes: `_data`, `_idx`, `_content`, `_segments`, `_segdir`,
/// `_docsize`, `_config`, `_ai`, `_au`, `_ad`).
///
/// Narrower than `fts_%` (pass-35Y review F2): a user-named
/// table like `fts_metrics` or `fts_custom_index` won't be
/// flagged. The canonical set is sourced from
/// `crates/mcp-agent-mail-db/src/schema.rs` (the pre-V3 FTS5
/// declarations).
pub const LEGACY_FTS_PREFIXES: &[&str] = &["fts_messages", "fts_agents", "fts_projects"];

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
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` drops the {count} residual object(s) via Op::DbExec in dependency order (TRIGGER → VIEW → TABLE). The chokepoint backs up the whole DB file first, so `am doctor undo <run-id>` restores the pre-fix DB byte-identically."
                ),
                "drop_sequence": build_drop_sql(&self.residual_objects),
                "manual_remediation": {
                    "steps": [
                        "Auto-fix (preferred): `am doctor fix --only fm-db-state-files-legacy-fts-residue --yes`. Drops the residual fts_* objects in dependency order via Op::DbExec; the chokepoint's DB-file backup makes `am doctor undo <run-id>` reverse it wholesale.",
                        "Before invoking on the live pooled DB: `PRAGMA wal_checkpoint(TRUNCATE);` and quiesce writers so the main DB file (not the -wal sidecar) carries the change and the backup is complete.",
                        "Manual alternative: drop triggers first (DROP TRIGGER IF EXISTS <name>;), then views (DROP VIEW IF EXISTS <name>;), then tables (DROP TABLE IF EXISTS <name>;) — order matters for dependency reasons.",
                        "Re-run `am doctor fix --only fm-db-state-files-legacy-fts-residue --list` to confirm sqlite_master is clean.",
                    ],
                    "residue_summary": object_summary,
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: count,
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

/// Read all `sqlite_master` rows whose `name` matches one of the
/// canonical legacy FTS5 prefixes (pass-35Y review F2 — narrowed
/// from the previous `fts_%` glob to avoid false-positive matches
/// against user-named tables like `fts_metrics`).
///
/// Returns `Some(rows)` on success (possibly empty), `None` on
/// query error.
fn read_fts_residue(conn: &sqlmodel_sqlite::SqliteConnection) -> Option<Vec<ResidualObject>> {
    let mut out = Vec::new();
    for prefix in LEGACY_FTS_PREFIXES {
        // Use a parameterized LIKE pattern. The `||` SQL operator
        // concatenates the parameter with `%`. The detector
        // matches `<prefix>`, `<prefix>_data`, `<prefix>_ai`,
        // `<prefix>_au`, `<prefix>_ad`, and all other FTS5
        // shadow-table / trigger suffixes.
        let pattern = format!("{prefix}%");
        let rows = conn
            .query_sync(
                "SELECT type, name FROM sqlite_master \
                 WHERE name LIKE ?1 AND type IN ('table', 'trigger', 'view')",
                &[sqlmodel_core::Value::Text(pattern)],
            )
            .ok()?;
        for r in rows {
            let kind = r.get_named::<String>("type").ok()?;
            let name = r.get_named::<String>("name").ok()?;
            out.push(ResidualObject { kind, name });
        }
    }
    Some(out)
}

/// Quote a SQLite identifier: wrap in double-quotes and double any
/// embedded double-quote. Defends against names from sqlite_master
/// that contain special characters (FTS5 names are simple in
/// practice, but never interpolate an unquoted identifier into DDL).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Build the dependency-ordered DROP sequence: TRIGGER first, then
/// VIEW, then TABLE. Each statement is `DROP <KIND> IF EXISTS
/// "<name>";` so the sequence is idempotent (re-running drops
/// nothing on the second pass). Objects with an unrecognized kind
/// are skipped (defensive — the detector only emits table / trigger
/// / view).
fn build_drop_sql(objects: &[ResidualObject]) -> String {
    // Drop priority by kind: lower number → dropped first.
    fn priority(kind: &str) -> Option<u8> {
        match kind {
            "trigger" => Some(0),
            "view" => Some(1),
            "table" => Some(2),
            _ => None,
        }
    }
    let mut ordered: Vec<&ResidualObject> = objects
        .iter()
        .filter(|o| priority(&o.kind).is_some())
        .collect();
    ordered.sort_by_key(|o| priority(&o.kind).unwrap_or(u8::MAX));
    let mut sql = String::new();
    for obj in ordered {
        let keyword = match obj.kind.as_str() {
            "trigger" => "TRIGGER",
            "view" => "VIEW",
            "table" => "TABLE",
            _ => continue,
        };
        sql.push_str(&format!(
            "DROP {keyword} IF EXISTS {};\n",
            quote_ident(&obj.name)
        ));
    }
    sql
}

/// Fixer. Routes through `mutate()` with a single `Op::DbExec`
/// carrying the dependency-ordered DROP sequence.
///
/// Skip semantics:
/// - DB path vanished between detect and fix → `actions_skipped`.
/// - No droppable objects (all had unrecognized kinds, or the
///   finding was empty) → `actions_skipped`.
///
/// Reversibility: the chokepoint backs up the whole DB file before
/// executing, so `am doctor undo <run-id>` restores it byte-identical.
pub fn fix(
    ctx: &MutateContext,
    finding: &LegacyFtsResidueFinding,
) -> Result<FixOutcome, MutateError> {
    if !finding.db_path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    let sql = build_drop_sql(&finding.residual_objects);
    if sql.trim().is_empty() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    mutate(ctx, &finding.db_path, Op::DbExec { sql })?;
    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
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
            fs::write(
                &marker_path,
                r#"{"version":1,"frankensearch_managed":true}"#,
            )
            .unwrap();
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

    /// **NEGATIVE TEST** (pass-35Y review F2): a user-named table
    /// with the prefix `fts_` but NOT one of the canonical legacy
    /// table names must NOT be flagged. The narrower filter from
    /// pass-35Y replaces the previous over-broad `fts_%` glob.
    #[test]
    fn detector_skips_user_named_fts_lookalike_tables() {
        let td = TempDir::new().unwrap();
        let db_path = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db_path.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);")
            .expect("create main table");
        // User-named tables with `fts_` prefix but NOT canonical
        // legacy names. The previous `fts_%` glob would have
        // flagged these.
        conn.execute_raw("CREATE TABLE fts_metrics (id INTEGER, value REAL);")
            .expect("create user table fts_metrics");
        conn.execute_raw("CREATE TABLE fts_my_custom (id INTEGER);")
            .expect("create user table fts_my_custom");
        conn.execute_raw("CREATE TABLE fts_sync_state (k TEXT, v TEXT);")
            .expect("create user table fts_sync_state");
        drop(conn);

        let marker_path = td.path().join("search_index").join(V3_MANAGED_MARKER);
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(&marker_path, r#"{"version":1}"#).unwrap();

        let findings = detect(&[db_path]);
        assert!(
            findings.is_empty(),
            "user-named fts_* tables must NOT be flagged after pass-35Y narrowing; got {} finding(s)",
            findings.len()
        );
    }

    /// **POSITIVE**: V3 active + residue present → flag.
    #[test]
    fn detector_flags_v3_active_db_with_fts_residue() {
        let td = TempDir::new().unwrap();
        let (db, marker) = setup_db(&td, /*residue=*/ true, /*v3_marker=*/ true);
        let findings = detect(std::slice::from_ref(&db));
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
        assert!(s.contains("\"auto_fixable\":true"));
        assert!(s.contains("\"estimated_actions\":2"));
        assert!(s.contains("auto_fix_summary"));
        assert!(s.contains("DROP TRIGGER"));
        // The drop_sequence must order TRIGGER before TABLE.
        let drop_seq = g
            .evidence
            .get("drop_sequence")
            .and_then(|v| v.as_str())
            .unwrap();
        let trigger_pos = drop_seq.find("DROP TRIGGER").unwrap();
        let table_pos = drop_seq.find("DROP TABLE").unwrap();
        assert!(
            trigger_pos < table_pos,
            "trigger must be dropped before table: {drop_seq}"
        );
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

    fn ctx_for(td: &TempDir, run_id: &str) -> crate::doctor::mutate::MutateContext {
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        crate::doctor::mutate::MutateContext {
            run_id: run_id.into(),
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
        }
    }

    /// `quote_ident` doubles embedded double-quotes.
    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("fts_messages"), "\"fts_messages\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    /// `build_drop_sql` orders TRIGGER → VIEW → TABLE regardless of
    /// input order, and skips unrecognized kinds.
    #[test]
    fn build_drop_sql_orders_by_dependency() {
        let objects = vec![
            ResidualObject {
                kind: "table".into(),
                name: "fts_messages".into(),
            },
            ResidualObject {
                kind: "view".into(),
                name: "fts_agents_v".into(),
            },
            ResidualObject {
                kind: "trigger".into(),
                name: "fts_messages_ai".into(),
            },
            ResidualObject {
                kind: "index".into(), // unrecognized → skipped
                name: "fts_messages_idx".into(),
            },
        ];
        let sql = build_drop_sql(&objects);
        let trigger = sql.find("DROP TRIGGER").unwrap();
        let view = sql.find("DROP VIEW").unwrap();
        let table = sql.find("DROP TABLE").unwrap();
        assert!(trigger < view, "trigger before view");
        assert!(view < table, "view before table");
        assert!(
            !sql.contains("fts_messages_idx"),
            "unrecognized kind (index) must be skipped"
        );
        assert!(sql.contains("DROP TRIGGER IF EXISTS \"fts_messages_ai\";"));
    }

    /// **NEGATIVE**: DB path vanished between detect and fix →
    /// skipped, never errors.
    #[test]
    fn fixer_skips_when_db_path_vanished() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__fts_vanished");
        let finding = LegacyFtsResidueFinding {
            db_path: td.path().join("nonexistent.sqlite3"),
            v3_marker_path: td.path().join("search_index").join(V3_MANAGED_MARKER),
            residual_objects: vec![ResidualObject {
                kind: "table".into(),
                name: "fts_messages".into(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: a finding whose objects all have unrecognized
    /// kinds yields an empty DROP sequence → skipped.
    #[test]
    fn fixer_skips_when_drop_sequence_empty() {
        let td = TempDir::new().unwrap();
        let (db_path, _marker) = setup_db(&td, true, true);
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__fts_empty_seq");
        let finding = LegacyFtsResidueFinding {
            db_path,
            v3_marker_path: td.path().join("search_index").join(V3_MANAGED_MARKER),
            residual_objects: vec![ResidualObject {
                kind: "index".into(), // unrecognized
                name: "fts_messages_idx".into(),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// Positive: fix() drops the residual objects from a real DB.
    /// Detector confirms zero residue afterward.
    #[test]
    fn fixer_drops_residue_via_db_exec() {
        let td = TempDir::new().unwrap();
        let (db_path, _marker) = setup_db(&td, true, true);

        // Pre-fix: detector finds residue.
        let pre = detect(std::slice::from_ref(&db_path));
        assert_eq!(pre.len(), 1, "detector must find residue pre-fix");
        let finding = pre.into_iter().next().unwrap();
        assert!(!finding.residual_objects.is_empty());

        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__fts_drop");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        // Post-fix: detector finds nothing.
        let post = detect(std::slice::from_ref(&db_path));
        assert!(
            post.is_empty(),
            "detector must find zero residue after fix: {post:?}"
        );
    }

    /// Idempotence: re-running on an already-clean DB drops nothing
    /// (DROP ... IF EXISTS is a no-op) and doesn't error.
    #[test]
    fn fixer_is_idempotent_on_clean_db() {
        let td = TempDir::new().unwrap();
        let (db_path, _marker) = setup_db(&td, true, true);
        let finding = detect(std::slice::from_ref(&db_path))
            .into_iter()
            .next()
            .unwrap();

        let ctx1 = ctx_for(&td, "2026-05-20T00-00-00Z__fts_idem_1");
        assert_eq!(fix(&ctx1, &finding).expect("fix 1").actions_taken, 1);

        // Second run with the SAME finding (stale residual list):
        // every DROP ... IF EXISTS is now a no-op. fix() still
        // reports actions_taken: 1 because it issued the Op::DbExec,
        // but the DB is unchanged and the detector stays clean.
        let ctx2 = ctx_for(&td, "2026-05-20T00-00-00Z__fts_idem_2");
        let outcome2 = fix(&ctx2, &finding).expect("fix 2");
        assert_eq!(outcome2.actions_taken, 1);
        assert!(detect(std::slice::from_ref(&db_path)).is_empty());
    }
}
