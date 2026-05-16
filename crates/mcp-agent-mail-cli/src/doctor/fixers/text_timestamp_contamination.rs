//! `fm-db-state-files-text-timestamp-contamination` — P0.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! One or more SQLite timestamp columns hold values whose storage
//! class is NOT `INTEGER` (and not `NULL`). The Rust server stores
//! every timestamp as `i64` microseconds since Unix epoch; the
//! pre-port Python server (mcp_agent_mail) stored timestamps as
//! ISO-8601 `TEXT`. If both servers ever touched the same DB file
//! (e.g., an operator ran the Python server against the same
//! `storage.sqlite3`, or restored a Python backup into the Rust
//! storage root), the TEXT timestamps poison every query that
//! expects `i64` arithmetic, causing silent comparison errors,
//! wrong ack-overdue calculations, and broken inbox filters.
//!
//! The Rust pool runs a boot-time migration in
//! `mcp_agent_mail_db::migrate` that converts known TEXT rows to
//! microseconds. This FM is the **on-demand re-poll** for that
//! same check: an operator can run `am doctor` (no restart
//! required) and see exactly which columns are contaminated and
//! how many rows.
//!
//! ## Detection (pure function)
//!
//! For each `(table, column)` in
//! `mcp_agent_mail_db::migrate::TIMESTAMP_COLUMNS`:
//!
//! ```sql
//! SELECT COUNT(*) FROM <table> WHERE typeof(<column>) NOT IN ('integer', 'null')
//! ```
//!
//! Any column with a positive count emits a row in the finding's
//! `contaminated_columns` evidence. The finding is detect-only:
//! the recovery path is either an `am serve` restart (which re-
//! runs the boot migration), or an explicit
//! `am doctor reconstruct` from the git archive.
//!
//! Round-10-review note: the detector opens the DB **read-only**
//! and **without follow-symlinks** semantics — it never touches
//! `-wal` / `-shm` sidecars and never holds a writer lock. Safe
//! to run against a live system.
//!
//! ## Fix
//!
//! **Detect-only.** The boot migration is the canonical fix path
//! and the doctor surface intentionally does not duplicate it
//! (Op::DbExec'ing the same SQL would race with the live server
//! if `am serve` is up). Manual remediation pointer: "Stop `am
//! serve` and restart it, or run `am doctor reconstruct`."

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use mcp_agent_mail_db::migrate::TIMESTAMP_COLUMNS;
use serde::Serialize;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-text-timestamp-contamination";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// One column's contamination summary.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContaminatedColumn {
    pub table: String,
    pub column: String,
    /// Number of rows whose `typeof(column)` is neither
    /// `'integer'` nor `'null'` (typically `'text'` from the
    /// pre-port Python server).
    pub non_integer_rows: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextTimestampContaminationFinding {
    pub db_path: PathBuf,
    pub contaminated_columns: Vec<ContaminatedColumn>,
    /// Total contaminated rows across all columns.
    pub total_rows: i64,
}

impl TextTimestampContaminationFinding {
    pub fn to_finding(&self) -> super::Finding {
        let col_count = self.contaminated_columns.len();
        let title = format!(
            "TEXT-typed timestamps in {} column{} ({} rows total) in {}; Python writer left contamination — boot migration or `am doctor reconstruct` required",
            col_count,
            if col_count == 1 { "" } else { "s" },
            self.total_rows,
            self.db_path.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "contaminated_columns": self.contaminated_columns,
                "total_rows": self.total_rows,
                "recovery_paths": [
                    "Restart `am serve` (re-runs boot migration)",
                    "`am doctor reconstruct` (rebuilds DB from git archive)",
                ],
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only — the boot migration is the
                // canonical fix; auto-fix would race with a live
                // server.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "DB {} has {} TEXT-typed rows across {} timestamp column(s). Either (a) restart \
             `am serve` so the boot-time migration converts them to i64 microseconds, or \
             (b) run `am doctor reconstruct` to rebuild the DB from the git archive. \
             Auto-fix is detect-only because running the conversion SQL while a live server \
             is writing would race.",
            self.db_path.display(),
            self.total_rows,
            self.contaminated_columns.len(),
        )
    }
}

/// Detector. PURE w.r.t. inputs; opens the DB read-only and runs
/// one `SELECT COUNT(*)` per timestamp column. Skips DBs that
/// can't be opened (missing, corrupted, or schema doesn't have the
/// table — different FMs handle those).
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<TextTimestampContaminationFinding> {
    let mut out = Vec::new();
    for db in candidate_dbs {
        if let Some(finding) = detect_one(db) {
            out.push(finding);
        }
    }
    out
}

fn detect_one(db_path: &std::path::Path) -> Option<TextTimestampContaminationFinding> {
    // Pass-35-review Gemini F1 (P1): opening a WAL-mode DB with
    // plain `read_only` flags can still create the `-shm`
    // sidecar (SQLite uses shared-memory tracking for WAL read
    // pointers). Use URI filename + `immutable=1` so SQLite
    // treats the file as truly immutable: no locking, no -shm
    // creation, no journal/WAL replay. This preserves the pure-
    // detector contract.
    let uri = super::sqlite_immutable_uri(db_path);
    let mut flags = OpenFlags::read_only();
    flags.uri = true;
    let config = SqliteConfig::file(uri).flags(flags);
    let conn = SqliteConnection::open(&config).ok()?;

    let mut contaminated = Vec::new();
    let mut total: i64 = 0;
    for &(table, column, _nullable) in TIMESTAMP_COLUMNS {
        // The query MUST use identifiers literally (table/column
        // names are compile-time constants); no parameter binding
        // applies. Validate that the identifiers are safe ASCII
        // word chars before formatting into SQL.
        if !is_safe_sql_ident(table) || !is_safe_sql_ident(column) {
            continue;
        }
        // Pass-35-review Codex F2 (P2): the FM is specifically
        // about TEXT contamination from the pre-port Python
        // writer. Narrow the query to `typeof = 'text'` so REAL
        // or BLOB contamination (different root cause —
        // possibly a corrupted vacuum, not a Python writer)
        // doesn't get misrouted to this remediation. The boot
        // migration in `mcp_agent_mail_db::migrate` ALSO only
        // converts TEXT → microseconds, so anchoring the
        // detector to TEXT keeps the contract aligned.
        let sql = format!("SELECT COUNT(*) AS n FROM {table} WHERE typeof({column}) = 'text'");
        let rows = match conn.query_sync(&sql, &[]) {
            Ok(r) => r,
            // The table may not exist on a freshly-created DB
            // (boot migration hasn't run yet) or on a partially-
            // populated test DB. Skip silently per-column rather
            // than abandon the whole scan.
            Err(_) => continue,
        };
        let n: i64 = rows
            .first()
            .and_then(|r| r.get_named::<i64>("n").ok())
            .unwrap_or(0);
        if n > 0 {
            contaminated.push(ContaminatedColumn {
                table: table.to_string(),
                column: column.to_string(),
                non_integer_rows: n,
            });
            total = total.saturating_add(n);
        }
    }

    if contaminated.is_empty() {
        return None;
    }

    Some(TextTimestampContaminationFinding {
        db_path: db_path.to_path_buf(),
        contaminated_columns: contaminated,
        total_rows: total,
    })
}

/// Defense-in-depth: `TIMESTAMP_COLUMNS` is a compile-time list of
/// internal table/column names, but we still validate against
/// `[A-Za-z0-9_]+` before formatting into SQL so a future addition
/// can never accidentally introduce an injection vector.
fn is_safe_sql_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &TextTimestampContaminationFinding,
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

    /// Build a minimal `storage.sqlite3` with the relevant tables
    /// + columns (matching what TIMESTAMP_COLUMNS expects).
    ///
    /// The real schema is in `mcp_agent_mail_db::schema`; for the
    /// detector test we only need column types to exist, not full
    /// FK / index integrity.
    fn make_minimal_db(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // Just create the tables that TIMESTAMP_COLUMNS references
        // with the relevant timestamp columns as ANY-typed (SQLite
        // is dynamically typed, so we can insert TEXT into them).
        for (table, column, _) in TIMESTAMP_COLUMNS {
            // CREATE TABLE IF NOT EXISTS — duplicates collapse.
            let _ = conn.execute_raw(&format!(
                "CREATE TABLE IF NOT EXISTS {table} ({column} INTEGER, _placeholder INTEGER)"
            ));
        }
        drop(conn);
        db
    }

    #[test]
    fn detector_returns_empty_for_clean_db() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_db(&td);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_text_contamination() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_db(&td);
        // Inject a TEXT timestamp into the `messages.created_ts`
        // column — SQLite's dynamic typing allows this even on a
        // column declared INTEGER.
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "INSERT INTO messages (created_ts, _placeholder) VALUES ('2025-01-15T10:30:00Z', 1)",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages (created_ts, _placeholder) VALUES ('2025-02-20T08:45:00Z', 2)",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].total_rows, 2);
        // contaminated_columns includes messages.created_ts.
        let has_messages_col = findings[0]
            .contaminated_columns
            .iter()
            .any(|c| c.table == "messages" && c.column == "created_ts");
        assert!(
            has_messages_col,
            "messages.created_ts contamination missing"
        );
    }

    #[test]
    fn detector_ignores_integer_timestamps() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_db(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // Healthy i64 microseconds — should NOT flag.
        conn.execute_raw(
            "INSERT INTO messages (created_ts, _placeholder) VALUES (1736937000000000, 1)",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty(), "INTEGER timestamp must not flag");
    }

    #[test]
    fn detector_ignores_null_timestamps() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_db(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("INSERT INTO message_recipients (read_ts, _placeholder) VALUES (NULL, 1)")
            .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty(), "NULL timestamp must not flag");
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nonexistent.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_db_with_no_matching_tables() {
        // A SQLite DB that doesn't have any of the
        // TIMESTAMP_COLUMNS tables → no contamination.
        let td = TempDir::new().unwrap();
        let db = td.path().join("empty.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE unrelated (x INTEGER)")
            .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_aggregates_contamination_across_columns() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_db(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "INSERT INTO messages (created_ts, _placeholder) VALUES ('2025-01-15T10:30:00Z', 1)",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents (inception_ts, _placeholder) VALUES ('2025-01-01T00:00:00Z', 1)",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].total_rows, 2);
        assert!(findings[0].contaminated_columns.len() >= 2);
    }

    #[test]
    fn is_safe_sql_ident_rejects_quotes_and_specials() {
        assert!(is_safe_sql_ident("messages"));
        assert!(is_safe_sql_ident("created_ts"));
        assert!(is_safe_sql_ident("Agent_Links_42"));
        assert!(!is_safe_sql_ident(""));
        assert!(!is_safe_sql_ident("foo;bar"));
        assert!(!is_safe_sql_ident("foo'bar"));
        assert!(!is_safe_sql_ident("foo bar"));
        assert!(!is_safe_sql_ident("foo-bar"));
    }

    #[test]
    fn finding_is_p0_detect_only_with_recovery_paths() {
        let f = TextTimestampContaminationFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            contaminated_columns: vec![ContaminatedColumn {
                table: "messages".to_string(),
                column: "created_ts".to_string(),
                non_integer_rows: 3,
            }],
            total_rows: 3,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "db_state_files");
        assert!(!g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("recovery_paths"));
        assert!(s.contains("am doctor reconstruct"));
    }
}
