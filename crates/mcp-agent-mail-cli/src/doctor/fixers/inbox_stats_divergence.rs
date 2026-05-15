//! `fm-db-state-files-inbox-stats-divergence` — P1.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `inbox_stats` is the materialized aggregate (per-agent
//! `total_count`, `unread_count`, `ack_pending_count`,
//! `last_message_ts`) used to render inbox dashboards without
//! scanning `message_recipients`. Triggers keep it in sync.
//!
//! Drift can creep in when:
//! - A trigger DROP'd during migration / restore wasn't re-
//!   created (the schema introduces multiple triggers; one
//!   missing leaves a partial count path).
//! - A direct SQL repair touched `message_recipients` outside
//!   the trigger surface (e.g., a Python writer running against
//!   the same DB before pass-29 hardening).
//! - A crash mid-mutation left `inbox_stats` partially updated.
//!
//! The detector compares `inbox_stats.unread_count` against the
//! ground-truth `COUNT(*) WHERE read_ts IS NULL` per agent. If
//! they disagree for any agent, emit a finding.
//!
//! ## Detection (pure function)
//!
//! Open the DB read-only (URI `?immutable=1` so a WAL probe
//! cannot create `-shm`). Run a single JOIN comparing stored
//! vs ground-truth unread counts. Surface up to 100 divergent
//! agents in the finding evidence.
//!
//! ## Fix (`Op::DbExec`)
//!
//! Rebuild from ground truth via the chokepoint:
//!
//! ```sql
//! DELETE FROM inbox_stats;
//! INSERT INTO inbox_stats
//!   (agent_id, total_count, unread_count, ack_pending_count, last_message_ts)
//! SELECT r.agent_id, COUNT(*),
//!        SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END),
//!        SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END),
//!        MAX(m.created_ts)
//! FROM message_recipients r
//! JOIN messages m ON m.id = r.message_id
//! GROUP BY r.agent_id;
//! ```
//!
//! Mirrors `mcp_agent_mail_db::sync::rebuild_agent_inbox_stats_sync`
//! but rebuilds ALL agents in one statement-pair rather than
//! per-agent (faster, single Op::DbExec).
//!
//! **Concurrency caveat**: if `am serve` is running, the rebuild
//! races with the running server's triggers. The manual
//! remediation envelope notes this; agents should stop the
//! server before invoking the fix.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use serde::Serialize;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-inbox-stats-divergence";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "db_state_files";

/// SQL used by the fixer. Public so doc tooling can surface the
/// exact statement an operator would otherwise have to type.
///
/// Pass-35L review (Codex F2 + Gemini F2 P0): wrapped in
/// `BEGIN IMMEDIATE` / `COMMIT` so a crash mid-statement cannot
/// leave the table empty (DELETE committed, INSERT not yet
/// applied). The `IMMEDIATE` mode also takes a write lock at
/// statement start, which prevents trigger races with a live
/// `am serve` (its INSERT-into-message_recipients trigger
/// would otherwise interleave between our DELETE and INSERT).
pub const REBUILD_SQL: &str = concat!(
    "BEGIN IMMEDIATE; ",
    "DELETE FROM inbox_stats; ",
    "INSERT INTO inbox_stats ",
    "(agent_id, total_count, unread_count, ack_pending_count, last_message_ts) ",
    "SELECT r.agent_id, COUNT(*), ",
    "SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END), ",
    "SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END), ",
    "MAX(m.created_ts) ",
    "FROM message_recipients r ",
    "JOIN messages m ON m.id = r.message_id ",
    "GROUP BY r.agent_id; ",
    "COMMIT;",
);

/// Single agent's divergence record.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DivergentAgent {
    pub agent_id: i64,
    pub stored_unread: i64,
    pub actual_unread: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxStatsDivergenceFinding {
    pub db_path: PathBuf,
    /// First 100 divergent agents (the SQL applies `LIMIT 100`).
    pub divergent_agents: Vec<DivergentAgent>,
    /// True if more than 100 agents are divergent (the slice was
    /// truncated by the LIMIT).
    pub more_truncated: bool,
}

impl InboxStatsDivergenceFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "inbox_stats diverges from ground truth for {} agent(s){} in {}",
            self.divergent_agents.len(),
            if self.more_truncated { "+" } else { "" },
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
                "divergent_agents": self.divergent_agents,
                "more_truncated": self.more_truncated,
                "rebuild_sql": REBUILD_SQL,
                "concurrency_caveat": "Stop `am serve` before applying the fix; live triggers will race the rebuild.",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "DB {} has {} agent(s) where `inbox_stats.unread_count` disagrees with the live \
             `message_recipients` count. Stop `am serve` first (live triggers would race the \
             rebuild), then run `am doctor fix --only {} --yes`. The fix is a single \
             DELETE + INSERT...SELECT rebuild routed through Op::DbExec; undo restores the \
             pre-rebuild bytes.",
            self.db_path.display(),
            self.divergent_agents.len(),
            FM_ID,
        )
    }
}

/// Detector. PURE w.r.t. caller-supplied DB paths.
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<InboxStatsDivergenceFinding> {
    let mut out = Vec::new();
    for db in candidate_dbs {
        if let Some(f) = detect_one(db) {
            out.push(f);
        }
    }
    out
}

fn detect_one(db_path: &std::path::Path) -> Option<InboxStatsDivergenceFinding> {
    // URI + immutable=1 — read-only, no -shm creation.
    let uri = format!("file:{}?immutable=1", db_path.to_string_lossy());
    let mut flags = OpenFlags::read_only();
    flags.uri = true;
    let config = SqliteConfig::file(uri).flags(flags);
    let conn = SqliteConnection::open(&config).ok()?;

    // Pass-35L review (Codex F1 + Gemini F1 P0): the pre-fix
    // detector drove from `inbox_stats LEFT JOIN aggregate`,
    // which silently missed the most important divergence
    // shape — agents with unread `message_recipients` rows but
    // NO matching `inbox_stats` row (e.g., a dropped INSERT
    // trigger after migration). The fix runs a `UNION ALL`
    // of two queries:
    //
    //   (A) inbox_stats LEFT JOIN gt: stored count disagrees
    //       with ground truth (or gt is missing → 0).
    //   (B) gt anti-joined against inbox_stats: ground-truth
    //       rows for agents with unread mail but no
    //       inbox_stats row at all (the missing-row case).
    //
    // Both rows share the `(aid, stored, actual)` shape so the
    // outer ResultSet decode loop is unchanged. `LIMIT 101`
    // (101 = 100 + sentinel for `more_truncated`) is applied
    // to the union.
    let query = concat!(
        "SELECT s.agent_id AS aid, s.unread_count AS stored, ",
        "       IFNULL(t.actual, 0) AS actual ",
        "FROM inbox_stats s ",
        "LEFT JOIN ( ",
        "  SELECT agent_id, COUNT(*) AS actual ",
        "  FROM message_recipients ",
        "  WHERE read_ts IS NULL ",
        "  GROUP BY agent_id ",
        ") t ON t.agent_id = s.agent_id ",
        "WHERE s.unread_count != IFNULL(t.actual, 0) ",
        "UNION ALL ",
        "SELECT t.agent_id AS aid, 0 AS stored, t.actual AS actual ",
        "FROM ( ",
        "  SELECT agent_id, COUNT(*) AS actual ",
        "  FROM message_recipients ",
        "  WHERE read_ts IS NULL ",
        "  GROUP BY agent_id ",
        ") t ",
        "WHERE t.actual > 0 ",
        "AND NOT EXISTS ( ",
        "  SELECT 1 FROM inbox_stats s WHERE s.agent_id = t.agent_id ",
        ") ",
        "LIMIT 101",
    );
    let rows = conn.query_sync(query, &[]).ok()?;
    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let more_truncated = total > 100;
    let take = total.min(100);
    let mut divergent = Vec::with_capacity(take);
    for row in rows.iter().take(take) {
        let agent_id = row.get_named::<i64>("aid").ok()?;
        let stored_unread = row.get_named::<i64>("stored").ok()?;
        let actual_unread = row.get_named::<i64>("actual").ok()?;
        divergent.push(DivergentAgent {
            agent_id,
            stored_unread,
            actual_unread,
        });
    }
    Some(InboxStatsDivergenceFinding {
        db_path: db_path.to_path_buf(),
        divergent_agents: divergent,
        more_truncated,
    })
}

/// Fixer — routes the rebuild SQL through the chokepoint via Op::DbExec.
///
/// The chokepoint takes a file-level backup before exec, records
/// before/after SHA-256 in actions.jsonl, and supports
/// `am doctor undo <run-id>` for byte-identical restore.
pub fn fix(
    ctx: &MutateContext,
    finding: &InboxStatsDivergenceFinding,
) -> Result<FixOutcome, MutateError> {
    let result = mutate(
        ctx,
        &finding.db_path,
        Op::DbExec {
            sql: REBUILD_SQL.to_string(),
        },
    )?;
    let mut outcome = FixOutcome::default();
    if result.ok {
        outcome.actions_taken += 1;
    } else {
        outcome.actions_skipped += 1;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::Capabilities;
    use std::fs::OpenOptions;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn make_ctx(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir,
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    /// Construct a minimal schema: messages + message_recipients +
    /// inbox_stats — just enough to exercise the detector / fixer.
    fn make_minimal_schema(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, ack_required INTEGER NOT NULL DEFAULT 0, created_ts INTEGER NOT NULL);
             CREATE TABLE message_recipients (
                 agent_id INTEGER NOT NULL,
                 message_id INTEGER NOT NULL,
                 read_ts INTEGER,
                 ack_ts INTEGER
             );
             CREATE TABLE inbox_stats (
                 agent_id INTEGER PRIMARY KEY,
                 total_count INTEGER NOT NULL DEFAULT 0,
                 unread_count INTEGER NOT NULL DEFAULT 0,
                 ack_pending_count INTEGER NOT NULL DEFAULT 0,
                 last_message_ts INTEGER
             );"
        )
        .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn detector_returns_empty_for_consistent_db() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_schema(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // 1 message, 1 unread recipient for agent 1, inbox_stats agrees.
        conn.execute_raw(
            "INSERT INTO messages (id, ack_required, created_ts) VALUES (1, 0, 1000);
             INSERT INTO message_recipients (agent_id, message_id, read_ts) VALUES (1, 1, NULL);
             INSERT INTO inbox_stats (agent_id, total_count, unread_count) VALUES (1, 1, 1);",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_stale_unread_count() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_schema(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // 3 unread recipients for agent 1 but inbox_stats says 1.
        conn.execute_raw(
            "INSERT INTO messages (id, ack_required, created_ts) VALUES (1, 0, 1000), (2, 0, 2000), (3, 0, 3000);
             INSERT INTO message_recipients (agent_id, message_id, read_ts) VALUES (1, 1, NULL), (1, 2, NULL), (1, 3, NULL);
             INSERT INTO inbox_stats (agent_id, total_count, unread_count) VALUES (1, 3, 1);",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].divergent_agents.len(), 1);
        assert_eq!(findings[0].divergent_agents[0].agent_id, 1);
        assert_eq!(findings[0].divergent_agents[0].stored_unread, 1);
        assert_eq!(findings[0].divergent_agents[0].actual_unread, 3);
        assert!(!findings[0].more_truncated);
    }

    #[test]
    fn detector_flags_missing_inbox_stats_row_for_agent_with_unread() {
        // Pass-35L review (Codex F1 + Gemini F1 P0): agents with
        // unread `message_recipients` but no `inbox_stats` row
        // at all (trigger drift / partial repair). Pre-fix this
        // was silently missed because the detector drove from
        // `inbox_stats LEFT JOIN gt`. The fix unions in an
        // anti-join branch.
        let td = TempDir::new().unwrap();
        let db = make_minimal_schema(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "INSERT INTO messages (id, ack_required, created_ts) VALUES (1, 0, 1000);
             INSERT INTO message_recipients (agent_id, message_id, read_ts) VALUES (99, 1, NULL);
             -- NOTE: no inbox_stats row for agent 99 (the drift scenario).",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        let div = &findings[0].divergent_agents;
        assert!(
            div.iter().any(|d| d.agent_id == 99 && d.stored_unread == 0 && d.actual_unread == 1),
            "missing-inbox_stats-row case must be surfaced; got: {div:?}",
        );
    }

    #[test]
    fn detector_handles_missing_recipients_for_stored_agent() {
        // Stored count > 0 but no recipients exist (e.g., all
        // were deleted). LEFT JOIN with IFNULL=0 should catch
        // this.
        let td = TempDir::new().unwrap();
        let db = make_minimal_schema(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw(
            "INSERT INTO inbox_stats (agent_id, total_count, unread_count) VALUES (42, 5, 5);",
        )
        .unwrap();
        drop(conn);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].divergent_agents[0].agent_id, 42);
        assert_eq!(findings[0].divergent_agents[0].stored_unread, 5);
        assert_eq!(findings[0].divergent_agents[0].actual_unread, 0);
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn fixer_rebuilds_inbox_stats_to_match_ground_truth() {
        let td = TempDir::new().unwrap();
        let db = make_minimal_schema(&td);
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        // Set up state where inbox_stats lies.
        conn.execute_raw(
            "INSERT INTO messages (id, ack_required, created_ts) VALUES (1, 0, 1000), (2, 1, 2000);
             INSERT INTO message_recipients (agent_id, message_id, read_ts, ack_ts) VALUES
                (7, 1, NULL, NULL),
                (7, 2, NULL, NULL);
             INSERT INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts)
                VALUES (7, 0, 0, 0, 0);",
        )
        .unwrap();
        drop(conn);
        let run_id = "2026-05-15T06-00-00Z__inbox-stats";
        let ctx = make_ctx(&td, run_id);
        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        drop(ctx);
        // After rebuild, detector should be clean.
        let findings_after = detect(std::slice::from_ref(&db));
        assert!(
            findings_after.is_empty(),
            "post-fix detector should be clean (got: {findings_after:?})"
        );
        // Verify ground truth: total=2, unread=2, ack_pending=1, last_ts=2000.
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        let row = conn
            .query_sync(
                "SELECT total_count AS t, unread_count AS u, ack_pending_count AS a, last_message_ts AS l FROM inbox_stats WHERE agent_id = 7",
                &[],
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("inbox_stats row for agent 7");
        assert_eq!(row.get_named::<i64>("t").unwrap(), 2);
        assert_eq!(row.get_named::<i64>("u").unwrap(), 2);
        assert_eq!(row.get_named::<i64>("a").unwrap(), 1);
        assert_eq!(row.get_named::<i64>("l").unwrap(), 2000);
    }

    #[test]
    fn finding_severity_is_p1_auto_fixable() {
        let f = InboxStatsDivergenceFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            divergent_agents: vec![DivergentAgent {
                agent_id: 1,
                stored_unread: 0,
                actual_unread: 3,
            }],
            more_truncated: false,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P1");
        assert!(g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("rebuild_sql"));
        assert!(s.contains("concurrency_caveat"));
    }
}
