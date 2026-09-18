//! Explicit, bounded sibling-project discovery from live task metadata.
//!
//! Suggestions are hints for human review, not links or contact permissions.
//! Ranking uses only project identity and bounded agent task descriptions; no
//! filesystem crawl, network request, LLM, or detached task is involved.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Row, Value};

use crate::{DbConn, DbError, DbPool, DbResult};

/// At most three persisted suggestions per refresh, matching the legacy budget.
pub const MAX_REFRESH_PAIRS: usize = 3;
/// Re-evaluate unreviewed suggestions no more than once every twelve hours.
pub const REFRESH_TTL_MICROS: i64 = 12 * 60 * 60 * 1_000_000;
const MAX_PROJECTS: i64 = 256;
const MAX_PROFILES: usize = 1024;
const MIN_SCORE: f64 = crate::queries::PROJECT_SIBLING_MIN_SUGGESTION_SCORE;

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct RefreshSummary {
    pub candidates: usize,
    pub written: usize,
    pub suggestions: Vec<DiscoveredPair>,
}

/// One persisted review hint. Scores are heuristic ranks, not probabilities.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DiscoveredPair {
    pub project_a_id: i64,
    pub project_b_id: i64,
    pub score: f64,
    pub rationale: String,
}

#[derive(Debug)]
struct Project {
    id: i64,
    slug: String,
    human_key: String,
    task_tokens: BTreeSet<String>,
}

#[derive(Debug)]
struct Candidate<'a> {
    a: &'a Project,
    b: &'a Project,
    score: f64,
    rationale: String,
}

fn db_error(error: impl std::fmt::Display) -> DbError {
    DbError::Sqlite(format!("sibling discovery: {error}"))
}

fn checkpoint(cx: &Cx, started: Instant) -> DbResult<()> {
    if cx.checkpoint().is_err() || started.elapsed() >= Duration::from_secs(2) {
        return Err(DbError::ResourceBusy(
            "sibling discovery cancelled or exceeded its refresh budget".to_string(),
        ));
    }
    Ok(())
}

/// Refresh hints through an already-admitted live pool.
///
/// Read-only diagnostics and static exports must not call this entrypoint.
/// `focus_project` includes that project even when it is outside the newest
/// 256 projects. This is explicit maintenance, not a page-render hook or a
/// replacement for the cheap creation-time seeding in `queries`.
/// A failed/cancelled refresh never advances a TTL.
/// Confirmed and dismissed decisions are retained until explicitly reset by
/// the existing review API; background heuristics cannot reverse a decision.
pub async fn refresh_project_sibling_suggestions(
    cx: &Cx,
    pool: &DbPool,
    focus_project: Option<i64>,
) -> Outcome<RefreshSummary, DbError> {
    if focus_project.is_some_and(|id| id <= 0) {
        return Outcome::Err(DbError::invalid(
            "project_id",
            "expected a positive project id",
        ));
    }
    if cx.checkpoint().is_err() {
        return Outcome::Cancelled(
            cx.cancel_reason()
                .unwrap_or_else(|| asupersync::CancelReason::user("sibling discovery cancelled")),
        );
    }
    if let Some(error) = crate::corruption_circuit_breaker().refusal_error() {
        return Outcome::Err(error);
    }
    // Hold the generation lease across identity reads and connection acquire,
    // not just COMMIT: a promotion must not replace the file between them.
    let _write_activity = crate::write_barrier::begin_write_activity();
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(panic) => return Outcome::Panicked(panic),
    };
    match refresh_conn(cx, &conn, focus_project, crate::now_micros()) {
        Ok(summary) => Outcome::Ok(summary),
        Err(error) => {
            crate::corruption_circuit_breaker().observe_error(&error);
            Outcome::Err(error)
        }
    }
}

fn project_from_row(row: &Row) -> DbResult<Project> {
    Ok(Project {
        id: row.get_named("id").map_err(db_error)?,
        slug: row.get_named("slug").map_err(db_error)?,
        human_key: row.get_named("human_key").map_err(db_error)?,
        task_tokens: BTreeSet::new(),
    })
}

fn refresh_conn(
    cx: &Cx,
    conn: &DbConn,
    focus_project: Option<i64>,
    now: i64,
) -> DbResult<RefreshSummary> {
    let started = Instant::now();
    checkpoint(cx, started)?;
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects ORDER BY id DESC LIMIT ?",
            &[Value::BigInt(MAX_PROJECTS)],
        )
        .map_err(db_error)?;
    let mut projects = rows
        .iter()
        .map(project_from_row)
        .collect::<DbResult<Vec<_>>>()?;
    if let Some(id) = focus_project
        && !projects.iter().any(|project| project.id == id)
    {
        checkpoint(cx, started)?;
        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key FROM projects WHERE id = ?",
                &[Value::BigInt(id)],
            )
            .map_err(db_error)?;
        if let Some(row) = rows.first() {
            projects.push(project_from_row(row)?);
        } else {
            return Err(DbError::not_found("Project", id.to_string()));
        }
    }
    if projects.len() < 2 {
        return Ok(RefreshSummary::default());
    }
    projects.sort_by_key(|project| project.id);
    let ids: Vec<Value> = projects
        .iter()
        .map(|project| Value::BigInt(project.id))
        .collect();
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    checkpoint(cx, started)?;
    let profile_sql = format!(
        "SELECT project_id, substr(task_description, 1, 256) AS task \
         FROM agents WHERE project_id IN ({placeholders}) AND retired_at IS NULL \
         AND NOT EXISTS (SELECT 1 FROM agent_deregistrations d WHERE d.agent_id = agents.id) \
         AND task_description <> '' ORDER BY last_active_ts DESC LIMIT {MAX_PROFILES}"
    );
    for row in conn.query_sync(&profile_sql, &ids).map_err(db_error)? {
        let id: i64 = row.get_named("project_id").map_err(db_error)?;
        let task: String = row.get_named("task").map_err(db_error)?;
        if let Ok(index) = projects.binary_search_by_key(&id, |project| project.id) {
            let remaining = 128_usize.saturating_sub(projects[index].task_tokens.len());
            projects[index]
                .task_tokens
                .extend(tokens(&task).into_iter().take(remaining));
        }
    }
    checkpoint(cx, started)?;
    let existing_sql = format!(
        "SELECT project_a_id, project_b_id, status, evaluated_ts, confirmed_ts, dismissed_ts \
         FROM project_sibling_suggestions WHERE project_a_id IN ({placeholders}) \
         AND project_b_id IN ({placeholders})"
    );
    let mut pair_ids = ids.clone();
    pair_ids.extend(ids);
    let cutoff = now.saturating_sub(REFRESH_TTL_MICROS);
    let mut blocked = BTreeSet::new();
    for row in conn
        .query_sync(&existing_sql, &pair_ids)
        .map_err(db_error)?
    {
        let a: i64 = row.get_named("project_a_id").map_err(db_error)?;
        let b: i64 = row.get_named("project_b_id").map_err(db_error)?;
        let status: String = row.get_named("status").map_err(db_error)?;
        let evaluated: i64 = row.get_named("evaluated_ts").map_err(db_error)?;
        let confirmed: Option<i64> = row.get_named("confirmed_ts").map_err(db_error)?;
        let dismissed: Option<i64> = row.get_named("dismissed_ts").map_err(db_error)?;
        if a > b
            || status != "suggested"
            || confirmed.is_some()
            || dismissed.is_some()
            || evaluated > cutoff
        {
            blocked.insert((a.min(b), a.max(b)));
        }
    }
    let mut candidates = Vec::new();
    for (index, a) in projects.iter().enumerate() {
        checkpoint(cx, started)?;
        for b in &projects[index + 1..] {
            if focus_project.is_some_and(|id| id != a.id && id != b.id)
                || blocked.contains(&(a.id, b.id))
            {
                continue;
            }
            if let Some((score, rationale)) = rank_pair(a, b) {
                candidates.push(Candidate {
                    a,
                    b,
                    score,
                    rationale,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| (a.a.id, a.b.id).cmp(&(b.a.id, b.b.id)))
    });
    let mut summary = RefreshSummary {
        candidates: candidates.len(),
        ..RefreshSummary::default()
    };
    candidates.truncate(MAX_REFRESH_PAIRS);
    if candidates.is_empty() {
        return Ok(summary);
    }
    checkpoint(cx, started)?;
    if let Some(error) = crate::corruption_circuit_breaker().refusal_error() {
        return Err(error);
    }
    let mut transaction = RefreshTransaction::begin(conn)?;
    for candidate in candidates {
        checkpoint(cx, started)?;
        let written = persist_candidate(conn, &candidate, now, cutoff)?;
        summary.written += written;
        if written == 1 {
            summary.suggestions.push(DiscoveredPair {
                project_a_id: candidate.a.id,
                project_b_id: candidate.b.id,
                score: candidate.score,
                rationale: candidate.rationale,
            });
        }
    }
    checkpoint(cx, started)?;
    transaction.commit()?;
    Ok(summary)
}

/// The conditional upsert is the review-race boundary. It rechecks current
/// status/TTL and exact project identities under the write transaction, not
/// just the earlier ranking snapshot. Never replace the row or its timestamps.
fn persist_candidate(
    conn: &DbConn,
    candidate: &Candidate<'_>,
    now: i64,
    cutoff: i64,
) -> DbResult<usize> {
    let before = candidate_revision(conn, candidate)?;
    conn.execute_sync(
        "INSERT INTO project_sibling_suggestions \
         (project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts) \
         SELECT ?, ?, ?, 'suggested', ?, ?, ? \
         WHERE EXISTS (SELECT 1 FROM projects WHERE id = ? AND human_key = ?) \
         AND EXISTS (SELECT 1 FROM projects WHERE id = ? AND human_key = ?) \
         AND NOT EXISTS (SELECT 1 FROM project_sibling_suggestions \
                         WHERE project_a_id = ? AND project_b_id = ?) \
         ON CONFLICT(project_a_id, project_b_id) DO UPDATE SET \
             score = excluded.score, rationale = excluded.rationale, evaluated_ts = excluded.evaluated_ts \
         WHERE project_sibling_suggestions.status = 'suggested' \
             AND project_sibling_suggestions.confirmed_ts IS NULL \
             AND project_sibling_suggestions.dismissed_ts IS NULL \
             AND project_sibling_suggestions.evaluated_ts <= ?",
        &[
            Value::BigInt(candidate.a.id),
            Value::BigInt(candidate.b.id),
            Value::Double(candidate.score),
            Value::Text(candidate.rationale.clone()),
            Value::BigInt(now),
            Value::BigInt(now),
            Value::BigInt(candidate.a.id),
            Value::Text(candidate.a.human_key.clone()),
            Value::BigInt(candidate.b.id),
            Value::Text(candidate.b.human_key.clone()),
            Value::BigInt(candidate.b.id),
            Value::BigInt(candidate.a.id),
            Value::BigInt(cutoff),
        ],
    )
    .map_err(db_error)?;
    // The runtime driver can report zero through changes() after a real
    // write. Read the row instead, under the caller's immediate transaction.
    let after = candidate_revision(conn, candidate)?;
    if after == before {
        return Ok(0);
    }
    match after {
        Some((id, evaluated, status, score, rationale))
            if evaluated == now
                && status == "suggested"
                && score.to_bits() == candidate.score.to_bits()
                && rationale.as_deref() == Some(candidate.rationale.as_str())
                && before.as_ref().is_none_or(|previous| previous.0 == id) =>
        {
            Ok(1)
        }
        _ => Err(db_error(
            "persisted sibling suggestion did not match the write witness",
        )),
    }
}

type CandidateRevision = (i64, i64, String, f64, Option<String>);

fn candidate_revision(
    conn: &DbConn,
    candidate: &Candidate<'_>,
) -> DbResult<Option<CandidateRevision>> {
    let rows = conn
        .query_sync(
            "SELECT id, evaluated_ts, status, score, rationale FROM project_sibling_suggestions \
             WHERE project_a_id = ? AND project_b_id = ?",
            &[Value::BigInt(candidate.a.id), Value::BigInt(candidate.b.id)],
        )
        .map_err(db_error)?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some((
            row.get_named("id").map_err(db_error)?,
            row.get_named("evaluated_ts").map_err(db_error)?,
            row.get_named("status").map_err(db_error)?,
            row.get_named("score").map_err(db_error)?,
            row.get_named("rationale").map_err(db_error)?,
        ))),
        _ => Err(db_error("multiple rows represent one sibling pair")),
    }
}

struct RefreshTransaction<'a> {
    conn: &'a DbConn,
    committed: bool,
}

impl<'a> RefreshTransaction<'a> {
    fn begin(conn: &'a DbConn) -> DbResult<Self> {
        conn.execute_raw("BEGIN IMMEDIATE").map_err(db_error)?;
        Ok(Self {
            conn,
            committed: false,
        })
    }

    fn commit(&mut self) -> DbResult<()> {
        self.conn.execute_raw("COMMIT").map_err(db_error)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for RefreshTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = self.conn.execute_raw("ROLLBACK")
        {
            tracing::warn!(%error, "sibling discovery rollback failed");
        }
    }
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= 3)
        .map(str::to_lowercase)
        .filter(|token| {
            !matches!(
                token.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "this"
                    | "that"
                    | "from"
                    | "into"
                    | "project"
                    | "repo"
                    | "rust"
                    | "python"
                    | "code"
                    | "work"
                    | "src"
                    | "implement"
                    | "implementation"
                    | "test"
                    | "tests"
                    | "agent"
                    | "agents"
            )
        })
        .take(128)
        .collect()
}

fn path_parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn rank_pair(a: &Project, b: &Project) -> Option<(f64, String)> {
    let a_path = path_parts(&a.human_key);
    let b_path = path_parts(&b.human_key);
    if a.id == b.id || a.human_key == b.human_key || a_path == b_path {
        return None;
    }
    let a_leaf = a_path.last().copied().unwrap_or(&a.slug);
    let b_leaf = b_path.last().copied().unwrap_or(&b.slug);
    let a_names = tokens(a_leaf);
    let b_names = tokens(b_leaf);
    let shared: Vec<_> = a_names.intersection(&b_names).cloned().collect();
    let mut score: f64 = 0.0;
    let mut evidence = Vec::new();
    if !shared.is_empty() {
        let union = a_names.union(&b_names).count();
        score = 0.90 + 0.07 * shared.len() as f64 / union as f64;
        evidence.push(format!("shared project-name terms: {}", shared.join(", ")));
    }
    if !a_names.is_empty() && a_leaf.eq_ignore_ascii_case(b_leaf) {
        score = score.max(0.98);
        evidence.push("same repository directory name in distinct locations".to_string());
    }
    if a_path.len() >= 2
        && b_path.len() >= 2
        && (a_path.starts_with(&b_path) || b_path.starts_with(&a_path))
    {
        score = score.max(0.96);
        evidence.push("one project directory contains the other".to_string());
    }
    let task_shared: Vec<_> = a.task_tokens.intersection(&b.task_tokens).collect();
    if task_shared.len() >= 3 {
        let union = a.task_tokens.union(&b.task_tokens).count();
        let overlap = task_shared.len() as f64 / union as f64;
        if overlap >= 0.5 {
            score = score.max(0.92 + 0.07 * overlap);
            evidence.push(format!("{} shared agent-task terms", task_shared.len()));
        }
    }
    // Merely living in /dp, /work, or the same user's home is not a relation.
    if score >= MIN_SCORE
        && a_path.len() >= 2
        && b_path.len() >= 2
        && a_path[..a_path.len() - 1] == b_path[..b_path.len() - 1]
    {
        score = (score + 0.02).min(1.0);
        evidence.push("shared parent directory".to_string());
    }
    (score >= MIN_SCORE).then(|| (score, format!("Local heuristic: {}", evidence.join("; "))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            slug: format!("project-{id}"),
            human_key: path.to_string(),
            task_tokens: BTreeSet::new(),
        }
    }

    fn fixture() -> DbConn {
        let conn = DbConn::open_memory().expect("real runtime SQLite");
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .expect("real mailbox schema");
        conn.execute_raw(
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES \
             (1, 'acme-api', '/work/acme-api', 1), (2, 'acme-web', '/work/acme-web', 1)",
        )
        .expect("fresh project rows, no seeded suggestion");
        conn
    }

    fn state(conn: &DbConn) -> Vec<Row> {
        conn.query_sync("SELECT * FROM project_sibling_suggestions ORDER BY id", &[])
            .unwrap()
    }

    #[test]
    fn names_paths_and_tasks_produce_explainable_hints() {
        let a = project(1, "/work/acme-api");
        let b = project(2, "/work/acme-web");
        let (score, rationale) = rank_pair(&a, &b).unwrap();
        assert!((MIN_SCORE..=1.0).contains(&score));
        assert!(rationale.contains("acme"));
        assert!(rank_pair(&a, &project(3, "/work/other-tool")).is_none());
        assert!(rank_pair(&a, &project(4, "/work/acme-api/")).is_none());
        let mut c = project(5, "/elsewhere/service");
        let mut d = project(6, "/different/dashboard");
        c.task_tokens = tokens("acme inventory reconciliation");
        d.task_tokens = tokens("acme inventory reconciliation dashboards");
        assert!(rank_pair(&c, &d).is_some());
    }

    #[test]
    fn fresh_mailbox_discovers_and_ttl_refresh_preserves_row_identity() {
        let conn = fixture();
        let cx = Cx::for_testing();
        let now = REFRESH_TTL_MICROS * 2;
        assert_eq!(refresh_conn(&cx, &conn, None, now).unwrap().written, 1);
        let first = state(&conn);
        let id: i64 = first[0].get_named("id").unwrap();
        assert_eq!(refresh_conn(&cx, &conn, None, now + 1).unwrap().written, 0);
        assert_eq!(
            refresh_conn(&cx, &conn, None, now + REFRESH_TTL_MICROS)
                .unwrap()
                .written,
            1
        );
        let after = state(&conn);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].get_named::<i64>("id").unwrap(), id);
        assert_eq!(after[0].get_named::<i64>("created_ts").unwrap(), now);
        assert_eq!(
            after[0].get_named::<i64>("evaluated_ts").unwrap(),
            now + REFRESH_TTL_MICROS
        );
    }

    #[test]
    fn terminal_review_decisions_survive_refresh_and_stale_ranking() {
        for status in ["confirmed", "dismissed"] {
            let conn = fixture();
            let cx = Cx::for_testing();
            let now = REFRESH_TTL_MICROS * 2;
            refresh_conn(&cx, &conn, None, now).unwrap();
            let a = project(1, "/work/acme-api");
            let b = project(2, "/work/acme-web");
            let (score, rationale) = rank_pair(&a, &b).unwrap();
            let candidate = Candidate {
                a: &a,
                b: &b,
                score,
                rationale,
            };
            conn.execute_sync(
                "UPDATE project_sibling_suggestions SET status = ?, confirmed_ts = 7, dismissed_ts = 9",
                &[Value::Text(status.to_string())],
            )
            .unwrap();
            assert_eq!(
                persist_candidate(&conn, &candidate, now * 10, now * 9).unwrap(),
                0
            );
            assert_eq!(refresh_conn(&cx, &conn, None, now * 10).unwrap().written, 0);
            let rows = state(&conn);
            assert_eq!(rows[0].get_named::<String>("status").unwrap(), status);
            assert_eq!(rows[0].get_named::<i64>("evaluated_ts").unwrap(), now);
            assert_eq!(rows[0].get_named::<i64>("confirmed_ts").unwrap(), 7);
            assert_eq!(rows[0].get_named::<i64>("dismissed_ts").unwrap(), 9);
        }
    }

    #[test]
    fn reverse_legacy_pair_is_not_duplicated() {
        let conn = fixture();
        conn.execute_raw(
            "INSERT INTO project_sibling_suggestions \
             (project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts) \
             VALUES (2, 1, 0.8, 'confirmed', 'operator', 1, 1)",
        )
        .unwrap();
        assert_eq!(
            refresh_conn(&Cx::for_testing(), &conn, None, REFRESH_TTL_MICROS * 2)
                .unwrap()
                .written,
            0
        );
        let rows = state(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("project_a_id").unwrap(), 2);
    }

    #[test]
    fn cancelled_refresh_and_query_only_connection_do_not_persist() {
        let conn = fixture();
        let cx = Cx::for_testing();
        cx.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel discovery"),
        );
        assert!(refresh_conn(&cx, &conn, None, REFRESH_TTL_MICROS * 2).is_err());
        assert!(state(&conn).is_empty());
        conn.execute_raw("PRAGMA query_only=ON").unwrap();
        assert!(refresh_conn(&Cx::for_testing(), &conn, None, REFRESH_TTL_MICROS * 2).is_err());
        assert!(state(&conn).is_empty());
    }

    #[test]
    fn refresh_is_bounded_and_focus_is_respected() {
        let conn = fixture();
        conn.execute_raw(
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES \
             (3, 'acme-worker', '/work/acme-worker', 1), \
             (4, 'acme-cli', '/work/acme-cli', 1), \
             (5, 'acme-docs', '/work/acme-docs', 1)",
        )
        .unwrap();
        let summary =
            refresh_conn(&Cx::for_testing(), &conn, Some(5), REFRESH_TTL_MICROS * 2).unwrap();
        assert_eq!(summary.written, MAX_REFRESH_PAIRS);
        assert!(summary.candidates > MAX_REFRESH_PAIRS);
        for row in state(&conn) {
            let a: i64 = row.get_named("project_a_id").unwrap();
            let b: i64 = row.get_named("project_b_id").unwrap();
            assert!(a < b);
            assert!(a == 5 || b == 5);
        }
    }

    #[test]
    fn abandoned_transaction_rolls_back_hint_and_ttl_together() {
        let conn = fixture();
        let a = project(1, "/work/acme-api");
        let b = project(2, "/work/acme-web");
        let (score, rationale) = rank_pair(&a, &b).unwrap();
        {
            let _transaction = RefreshTransaction::begin(&conn).unwrap();
            persist_candidate(
                &conn,
                &Candidate {
                    a: &a,
                    b: &b,
                    score,
                    rationale,
                },
                10,
                0,
            )
            .unwrap();
        }
        assert!(state(&conn).is_empty());
        conn.execute_raw("BEGIN IMMEDIATE; ROLLBACK")
            .expect("connection is usable after rollback");
    }

    #[test]
    fn project_identity_change_invalidates_a_prepared_candidate() {
        let conn = fixture();
        let a = project(1, "/work/acme-api");
        let b = project(2, "/work/acme-web");
        let (score, rationale) = rank_pair(&a, &b).unwrap();
        conn.execute_raw("UPDATE projects SET human_key = '/unrelated/replacement' WHERE id = 2")
            .unwrap();
        assert_eq!(
            persist_candidate(
                &conn,
                &Candidate {
                    a: &a,
                    b: &b,
                    score,
                    rationale,
                },
                10,
                0,
            )
            .unwrap(),
            0
        );
        assert!(state(&conn).is_empty());
    }

    #[test]
    fn persisted_suggestions_and_cooldown_survive_reopen() {
        let directory = tempfile::tempdir()
            .expect("retained real mailbox fixture")
            .keep();
        let path = directory.join("sibling-discovery.sqlite3");
        let conn = DbConn::open_file(path.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        conn.execute_raw(
            "INSERT INTO projects(id, slug, human_key, created_at) VALUES \
             (1, 'acme-api', '/work/acme-api', 1), (2, 'acme-web', '/work/acme-web', 1)",
        )
        .unwrap();
        let now = REFRESH_TTL_MICROS * 2;
        assert_eq!(
            refresh_conn(&Cx::for_testing(), &conn, None, now)
                .unwrap()
                .written,
            1
        );
        crate::close_db_conn(conn, "sibling discovery persistence fixture");
        let reopened = DbConn::open_file(path.to_string_lossy().as_ref()).unwrap();
        assert_eq!(state(&reopened).len(), 1);
        assert_eq!(
            refresh_conn(&Cx::for_testing(), &reopened, Some(1), now + 1)
                .unwrap()
                .written,
            0
        );
        assert_eq!(
            state(&reopened)[0]
                .get_named::<i64>("evaluated_ts")
                .unwrap(),
            now
        );
        crate::close_db_conn(reopened, "sibling discovery persistence reopen");
    }

    #[test]
    fn focused_project_outside_scan_window_is_still_discovered() {
        let conn = fixture();
        for id in 3..=260 {
            conn.execute_raw(&format!(
                "INSERT INTO projects(id, slug, human_key, created_at) \
                 VALUES ({id}, 'unrelated-{id}', '/elsewhere/item{id}', 1)"
            ))
            .unwrap();
        }
        conn.execute_raw(
            "INSERT INTO projects(id, slug, human_key, created_at) \
             VALUES (300, 'acme-jobs', '/work/acme-jobs', 1)",
        )
        .unwrap();
        let summary = refresh_conn(&Cx::for_testing(), &conn, Some(1), REFRESH_TTL_MICROS * 2)
            .expect("old project remains explicitly addressable");
        assert_eq!(summary.written, 1);
        assert_eq!(summary.suggestions[0].project_a_id, 1);
        assert_eq!(summary.suggestions[0].project_b_id, 300);
    }

    #[test]
    fn active_task_metadata_discovers_differently_named_projects() {
        let conn = fixture();
        conn.execute_raw(
            "UPDATE projects SET human_key = '/warehouse/dispatch', slug = 'dispatch' WHERE id = 1; \
             UPDATE projects SET human_key = '/store/shopfront', slug = 'shopfront' WHERE id = 2; \
             INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (1, 1, 'BlueLake', 'test', 'test', 'acme inventory reconciliation', 1, 1), \
                    (2, 2, 'RedStone', 'test', 'test', 'acme inventory reconciliation dashboards', 1, 1); \
             INSERT INTO agent_deregistrations (agent_id, deregistered_at) VALUES (2, 2)",
        )
        .unwrap();
        assert_eq!(
            refresh_conn(&Cx::for_testing(), &conn, None, REFRESH_TTL_MICROS * 2)
                .unwrap()
                .written,
            0,
            "deregistered profiles must not supply active task evidence"
        );
        // A new active agent supplies real task metadata. No suggestion row
        // is seeded by the fixture; discovery must generate it from the DB.
        conn.execute_raw(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (3, 2, 'GreenHill', 'test', 'test', 'acme inventory reconciliation dashboards', 1, 3)",
        )
        .unwrap();
        let summary =
            refresh_conn(&Cx::for_testing(), &conn, None, REFRESH_TTL_MICROS * 2).unwrap();
        assert_eq!(summary.written, 1);
        assert!(
            summary.suggestions[0].score >= crate::queries::PROJECT_SIBLING_MIN_SUGGESTION_SCORE
        );
        assert!(
            summary.suggestions[0]
                .rationale
                .contains("agent-task terms")
        );
    }

    #[test]
    fn missing_focused_project_is_not_an_empty_success() {
        let conn = fixture();
        assert!(matches!(
            refresh_conn(
                &Cx::for_testing(),
                &conn,
                Some(999_999),
                REFRESH_TTL_MICROS * 2,
            ),
            Err(DbError::NotFound { .. })
        ));
        assert!(state(&conn).is_empty());
    }
}
