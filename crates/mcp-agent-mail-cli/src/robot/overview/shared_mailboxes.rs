//! Count virtual shared deliveries without expanding messages by agent fanout.
//!
//! Eligible registration times form a sorted population per project. Each
//! message starts with that population at its send time, minus its sender.
//! Only stored direct deliveries and nonempty receipts can reduce that count.
//! The canonical DB-layer query remains the oracle and the dense/schema fallback.
//! No virtual recipient or receipt is ever written by this observation.

use super::*;

// Strategy limits, never result limits. Abandon the strategy BEFORE adding a
// page's counters when either bound is exceeded, then use the canonical query.
const MAX_SHARED_VIEWERS: usize = 4096;
// Bound recovery attempts, not results. A large fleet can contain many small
// shared mailboxes even when its combined agent population exceeds the cap.
const MAX_SCOPED_POPULATION_LOADS: usize = 4;
const VIEWERS_SQL: &str = "SELECT id, project_id, inception_ts FROM agents";
// Keep SQL's LOWER/COALESCE semantics, not Rust's Unicode lowercase rules.
// This is the agent-only portion of project_mailbox::visible_to_viewer_sql;
// differential native tests below exercise the canonical query, not a copy.
const VIEWERS_FILTER: &str = "retired_at IS NULL AND inception_ts IS NOT NULL
    AND LOWER(COALESCE(contact_policy, 'auto')) <> 'block_all'";
const SHARED_MESSAGES_SQL: &str = "SELECT id, project_id, sender_id, created_ts,
    CASE WHEN importance IN ('urgent', 'high') THEN 1 ELSE 0 END AS urgent,
    CASE WHEN ack_required = 1 AND created_ts < ? THEN 1 ELSE 0 END AS overdue
    FROM messages";
const DIRECT_SQL: &str = "SELECT message_id, agent_id FROM message_recipients";
const DIRECT_FILTER: &str = "agent_id IS NOT NULL";
const RECEIPTS_SQL: &str = "SELECT message_id, agent_id,
    CASE WHEN read_ts IS NOT NULL THEN 1 ELSE 0 END AS read_set,
    CASE WHEN ack_ts IS NOT NULL THEN 1 ELSE 0 END AS ack_set
    FROM project_mailbox_receipts";
const RECEIPTS_FILTER: &str = "agent_id IS NOT NULL
    AND (read_ts IS NOT NULL OR ack_ts IS NOT NULL)";

#[derive(Clone, Copy)]
struct Viewer {
    project_id: i64,
    inception_ts: i64,
}

struct Population {
    viewers: HashMap<i64, Viewer>,
    inception: HashMap<i64, Vec<i64>>,
    direct_index: String,
    receipt_index: String,
}

/// One collector per read snapshot, never a process-local or persistent cache.
#[derive(Default)]
pub(super) struct Collector {
    state: State,
}

#[derive(Default)]
enum State {
    #[default]
    Unprepared,
    Disabled,
    Ready(Population),
    Scoped(ScopedPopulation),
}

enum PopulationLoad {
    Ready(Population),
    Unavailable,
    Overflow {
        direct_index: String,
        receipt_index: String,
        oversized_projects: HashSet<i64>,
    },
}

/// Recovery for a fleet-wide population overflow. Keep at most one complete
/// page scope, including empty projects; never mistake an omitted project for
/// a project with no viewers. The cache belongs to the caller's read snapshot.
struct ScopedPopulation {
    agent_index: String,
    direct_index: String,
    receipt_index: String,
    oversized_projects: HashSet<i64>,
    attempted_scope: Option<HashSet<i64>>,
    population: Option<Population>,
    loads_left: usize,
}

struct Delta {
    project_id: i64,
    sender_id: i64,
    created_ts: i64,
    urgent: bool,
    overdue: bool,
    unread: usize,
    unacked: usize,
}

fn failed(message: &str) -> CliError {
    CliError::Other(format!("overview shared population {message}"))
}

/// A full UNIQUE two-column index proves pair identity, unlike an index that
/// merely starts with the pair or is unique only with an additional column.
fn pair_index(conn: &DbConn, table: &str) -> Result<Option<String>, CliError> {
    let indexes = conn
        .query_sync(
            &format!("PRAGMA index_list({})", quoted_identifier(table)),
            &[],
        )
        .map_err(|error| failed(&format!("index probe failed: {error}")))?;
    for index in indexes {
        if index.get_by_name("unique").and_then(Value::as_i64) != Some(1)
            || index.get_by_name("partial").and_then(Value::as_i64) != Some(0)
        {
            continue;
        }
        let Ok(name) = index.get_named::<String>("name") else {
            continue;
        };
        let columns = conn
            .query_sync(
                &format!("PRAGMA index_info({})", quoted_identifier(&name)),
                &[],
            )
            .map_err(|error| failed(&format!("index columns failed: {error}")))?;
        if columns.len() == 2
            && [(0, "message_id"), (1, "agent_id")]
                .iter()
                .all(|(position, expected)| {
                    columns.iter().any(|column| {
                        column.get_by_name("seqno").and_then(Value::as_i64) == Some(*position)
                            && column.get_named::<String>("name").ok().as_deref() == Some(*expected)
                    })
                })
        {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

impl Population {
    fn load(conn: &DbConn, work: &mut ScanWork) -> Result<PopulationLoad, CliError> {
        let Some(direct_index) = pair_index(conn, "message_recipients")? else {
            return Ok(PopulationLoad::Unavailable);
        };
        let Some(receipt_index) = pair_index(conn, "project_mailbox_receipts")? else {
            return Ok(PopulationLoad::Unavailable);
        };
        let mut viewers = HashMap::new();
        let mut inception: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut after = None;
        loop {
            let (continuation, params) = after.map_or_else(
                || ("", Vec::new()),
                |id| (" AND id > ?", vec![Value::BigInt(id)]),
            );
            let sql = format!(
                "{VIEWERS_SQL} WHERE ({VIEWERS_FILTER}){continuation} \
                 ORDER BY id LIMIT {OVERVIEW_PAGE_ROWS}"
            );
            let rows = bounded_query(conn, &sql, &params, work)?;
            // Result buffers remain one page. Retained population never exceeds
            // the cap; no partially loaded population can produce counts.
            if viewers.len() + rows.len() > MAX_SHARED_VIEWERS {
                // A project already over the cap cannot benefit from another
                // scan of that project. This is a lower bound from real rows,
                // not a guess about the unvisited population.
                let mut sizes: HashMap<_, _> = inception
                    .iter()
                    .map(|(id, times)| (*id, times.len()))
                    .collect();
                for row in &rows {
                    *sizes.entry(integer(row, "project_id")?).or_default() += 1;
                }
                let oversized_projects = sizes
                    .into_iter()
                    .filter_map(|(id, size)| (size > MAX_SHARED_VIEWERS).then_some(id))
                    .collect();
                return Ok(PopulationLoad::Overflow {
                    direct_index,
                    receipt_index,
                    oversized_projects,
                });
            }
            for row in &rows {
                let id = integer(row, "id")?;
                if after.is_some_and(|previous| id <= previous) {
                    return Err(failed("viewer cursor did not advance"));
                }
                after = Some(id);
                let viewer = Viewer {
                    project_id: integer(row, "project_id")?,
                    inception_ts: integer(row, "inception_ts")?,
                };
                inception
                    .entry(viewer.project_id)
                    .or_default()
                    .push(viewer.inception_ts);
                viewers.insert(id, viewer);
            }
            if rows.len() < OVERVIEW_PAGE_ROWS {
                break;
            }
        }
        for times in inception.values_mut() {
            times.sort_unstable();
        }
        Ok(PopulationLoad::Ready(Self {
            viewers,
            inception,
            direct_index,
            receipt_index,
        }))
    }

    fn eligible(&self, agent_id: i64, delta: &Delta) -> bool {
        agent_id != delta.sender_id
            && self.viewers.get(&agent_id).is_some_and(|viewer| {
                viewer.project_id == delta.project_id && viewer.inception_ts <= delta.created_ts
            })
    }

    fn page(
        &self,
        conn: &DbConn,
        deliveries: &[Row],
        threshold: i64,
        work: &mut ScanWork,
    ) -> Result<Option<Vec<Delta>>, CliError> {
        let mut delivery_projects = HashMap::with_capacity(deliveries.len());
        let mut ids = Vec::with_capacity(deliveries.len());
        for row in deliveries {
            let id = integer(row, "id")?;
            if delivery_projects
                .insert(id, integer(row, "project_id")?)
                .is_some()
            {
                return Err(failed("received duplicate delivery ids"));
            }
            ids.push(id);
        }
        if ids.is_empty() || ids.len() > OVERVIEW_PAGE_ROWS {
            return Err(failed("received an invalid delivery page"));
        }
        let marks = vec!["?"; ids.len()].join(",");
        let params: Vec<_> = ids.iter().copied().map(Value::BigInt).collect();
        // One sentinel row fits in the existing page budget. Exactly a full
        // page is inconclusive, even if it happens to be the entire result.
        let direct = bounded_query(
            conn,
            &format!(
                "{DIRECT_SQL} INDEXED BY {} WHERE message_id IN ({marks}) \
             AND ({DIRECT_FILTER}) LIMIT {OVERVIEW_PAGE_ROWS}",
                quoted_identifier(&self.direct_index),
            ),
            &params,
            work,
        )?;
        if direct.len() == OVERVIEW_PAGE_ROWS {
            return Ok(None);
        }
        let receipts = bounded_query(
            conn,
            &format!(
                "{RECEIPTS_SQL} INDEXED BY {} WHERE message_id IN ({marks}) \
             AND ({RECEIPTS_FILTER}) LIMIT {OVERVIEW_PAGE_ROWS}",
                quoted_identifier(&self.receipt_index),
            ),
            &params,
            work,
        )?;
        if receipts.len() == OVERVIEW_PAGE_ROWS {
            return Ok(None);
        }
        let metadata = lookup_ids(
            conn,
            SHARED_MESSAGES_SQL,
            "id",
            &[Value::BigInt(threshold)],
            &ids,
            work,
        )?;
        let mut deltas = HashMap::with_capacity(metadata.len());
        for row in metadata {
            let id = integer(&row, "id")?;
            let project_id = integer(&row, "project_id")?;
            if delivery_projects.get(&id) != Some(&project_id) {
                // Same inner-join and same-project rule as the canonical query.
                continue;
            }
            let (Some(sender_id), Some(created_ts)) = (
                row.get_by_name("sender_id").and_then(Value::as_i64),
                row.get_by_name("created_ts").and_then(Value::as_i64),
            ) else {
                // NULL comparisons cannot make a viewer eligible in SQL.
                // Refuse malformed non-NULL fields instead of guessing.
                for name in ["sender_id", "created_ts"] {
                    match row.get_by_name(name) {
                        Some(Value::Null | Value::Int(_) | Value::BigInt(_)) => {}
                        _ => return Err(failed("message eligibility field is not an integer")),
                    }
                }
                continue;
            };
            let population = self
                .inception
                .get(&project_id)
                .map_or(0, |times| times.partition_point(|time| *time <= created_ts));
            let mut delta = Delta {
                project_id,
                sender_id,
                created_ts,
                urgent: integer(&row, "urgent")? != 0,
                overdue: integer(&row, "overdue")? != 0,
                unread: population,
                unacked: population,
            };
            if self.viewers.get(&sender_id).is_some_and(|viewer| {
                viewer.project_id == project_id && viewer.inception_ts <= created_ts
            }) {
                delta.unread = delta
                    .unread
                    .checked_sub(1)
                    .ok_or_else(|| failed("sender underflow"))?;
                delta.unacked = delta.unread;
            }
            deltas.insert(id, delta);
        }
        let mut direct_pairs = HashSet::with_capacity(direct.len());
        for row in direct {
            let pair = (integer(&row, "message_id")?, integer(&row, "agent_id")?);
            if !direct_pairs.insert(pair) {
                return Err(failed("duplicate direct pair in a unique index"));
            }
            if let Some(delta) = deltas.get_mut(&pair.0)
                && self.eligible(pair.1, delta)
            {
                subtract(&mut delta.unread, true)?;
                subtract(&mut delta.unacked, true)?;
            }
        }
        for row in receipts {
            let pair = (integer(&row, "message_id")?, integer(&row, "agent_id")?);
            if direct_pairs.contains(&pair) {
                continue; // Direct delivery always wins, regardless of its state.
            }
            if let Some(delta) = deltas.get_mut(&pair.0)
                && self.eligible(pair.1, delta)
            {
                subtract(&mut delta.unread, integer(&row, "read_set")? != 0)?;
                subtract(&mut delta.unacked, integer(&row, "ack_set")? != 0)?;
            }
        }
        Ok(Some(deltas.into_values().collect()))
    }
}

impl ScopedPopulation {
    fn for_page(
        &mut self,
        conn: &DbConn,
        deliveries: &[Row],
        work: &mut ScanWork,
    ) -> Result<Option<&Population>, CliError> {
        if deliveries.is_empty() || deliveries.len() > OVERVIEW_PAGE_ROWS {
            return Err(failed("received an invalid scoped delivery page"));
        }
        let scope: HashSet<_> = deliveries
            .iter()
            .map(|row| integer(row, "project_id"))
            .collect::<Result<_, _>>()?;
        if !scope.is_disjoint(&self.oversized_projects) {
            return Ok(None);
        }
        if let Some(previous) = &self.attempted_scope
            && ((self.population.is_some() && scope.is_subset(previous)) || scope == *previous)
        {
            return Ok(self.population.as_ref());
        }
        if self.loads_left == 0 {
            return Ok(None);
        }
        self.loads_left -= 1;
        // Drop the old cache before allocating the replacement: the cap is
        // shared across projects, not multiplied by the number of page scopes.
        self.population = None;
        self.attempted_scope = Some(scope.clone());
        self.population = self.load_scope(conn, &scope, work)?;
        Ok(self.population.as_ref())
    }

    fn load_scope(
        &self,
        conn: &DbConn,
        scope: &HashSet<i64>,
        work: &mut ScanWork,
    ) -> Result<Option<Population>, CliError> {
        if full_index_with_prefix(conn, "agents", &["project_id"])?.as_deref()
            != Some(self.agent_index.as_str())
        {
            return Err(failed("verified scoped agent index is no longer available"));
        }
        let mut viewers = HashMap::new();
        let mut inception: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut project_ids: Vec<_> = scope.iter().copied().collect();
        project_ids.sort_unstable();
        for project_id in project_ids {
            let mut after = None;
            loop {
                let mut params = vec![Value::BigInt(project_id)];
                let continuation = after.map_or("", |id| {
                    params.push(Value::BigInt(id));
                    " AND id > ?"
                });
                // The verified leading project key excludes unrelated fleets
                // before sorting by id. Composite indexes may still sort this
                // one project's rows; the strategy does not claim zero VM work.
                let sql = format!(
                    "{VIEWERS_SQL} INDEXED BY {} WHERE ({VIEWERS_FILTER}) \
                     AND project_id = ?{continuation} ORDER BY id LIMIT {OVERVIEW_PAGE_ROWS}",
                    quoted_identifier(&self.agent_index),
                );
                let rows = bounded_query(conn, &sql, &params, work)?;
                if viewers.len() + rows.len() > MAX_SHARED_VIEWERS {
                    return Ok(None);
                }
                for row in &rows {
                    let id = integer(row, "id")?;
                    if integer(row, "project_id")? != project_id
                        || after.is_some_and(|previous| id <= previous)
                    {
                        return Err(failed(
                            "scoped viewer cursor did not advance within its project",
                        ));
                    }
                    after = Some(id);
                    let viewer = Viewer {
                        project_id,
                        inception_ts: integer(row, "inception_ts")?,
                    };
                    if viewers.insert(id, viewer).is_some() {
                        return Err(failed("duplicate scoped viewer id"));
                    }
                    inception
                        .entry(project_id)
                        .or_default()
                        .push(viewer.inception_ts);
                }
                if rows.len() < OVERVIEW_PAGE_ROWS {
                    break;
                }
            }
        }
        for times in inception.values_mut() {
            times.sort_unstable();
        }
        Ok(Some(Population {
            viewers,
            inception,
            direct_index: self.direct_index.clone(),
            receipt_index: self.receipt_index.clone(),
        }))
    }
}

fn subtract(total: &mut usize, settled: bool) -> Result<(), CliError> {
    if settled {
        *total = total
            .checked_sub(1)
            .ok_or_else(|| failed("correction underflow"))?;
    }
    Ok(())
}

impl Collector {
    fn prepare(conn: &DbConn, work: &mut ScanWork) -> Result<State, CliError> {
        match Population::load(conn, work)? {
            PopulationLoad::Ready(population) => Ok(State::Ready(population)),
            PopulationLoad::Unavailable => Ok(State::Disabled),
            PopulationLoad::Overflow {
                direct_index,
                receipt_index,
                oversized_projects,
            } => {
                let Some(agent_index) = full_index_with_prefix(conn, "agents", &["project_id"])?
                else {
                    return Ok(State::Disabled);
                };
                Ok(State::Scoped(ScopedPopulation {
                    agent_index,
                    direct_index,
                    receipt_index,
                    oversized_projects,
                    attempted_scope: None,
                    population: None,
                    loads_left: MAX_SCOPED_POPULATION_LOADS,
                }))
            }
        }
    }

    /// False leaves project counters untouched. A dense page disables this
    /// strategy for the rest of the invocation, so every later dense page does
    /// not pay another sentinel probe. The caller then uses the canonical query.
    pub(super) fn try_page(
        &mut self,
        conn: &DbConn,
        deliveries: &[Row],
        threshold: i64,
        projects: &mut HashMap<i64, OverviewProject>,
        work: &mut ScanWork,
    ) -> Result<bool, CliError> {
        if matches!(self.state, State::Unprepared) {
            self.state = Self::prepare(conn, work)?;
        }
        let population: &Population = match &mut self.state {
            State::Ready(population) => population,
            State::Scoped(scoped) => {
                let Some(population) = scoped.for_page(conn, deliveries, work)? else {
                    return Ok(false);
                };
                population
            }
            State::Unprepared | State::Disabled => return Ok(false),
        };
        let Some(deltas) = population.page(conn, deliveries, threshold, work)? else {
            self.state = State::Disabled;
            return Ok(false);
        };
        // Both correction sets were complete before any counters are published.
        for delta in deltas {
            let urgent = if delta.urgent { delta.unread } else { 0 };
            let overdue = if delta.overdue { delta.unacked } else { 0 };
            if delta.unread == 0 && overdue == 0 {
                continue;
            }
            let counts = project(projects, delta.project_id);
            for (total, add) in [
                (&mut counts.unread, delta.unread),
                (&mut counts.urgent, urgent),
                (&mut counts.ack_overdue, overdue),
            ] {
                *total = total
                    .checked_add(add)
                    .ok_or_else(|| failed("counter overflow"))?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NOW: i64 = 18_000_000_000;
    const SENT: i64 = NOW - ACK_OVERDUE_THRESHOLD_US - 1;

    fn execute(conn: &DbConn, sql: &str) {
        conn.execute_sync(sql, &[]).expect(sql);
    }

    fn fixture() -> (tempfile::TempDir, DbConn) {
        let dir = tempfile::tempdir().unwrap();
        let conn = DbConn::open_file(dir.path().join("shared.sqlite3").to_str().unwrap()).unwrap();
        for sql in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, inception_ts INTEGER, retired_at INTEGER, contact_policy TEXT)",
            "CREATE INDEX agents_project ON agents(project_id)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER, created_ts INTEGER, importance TEXT, ack_required INTEGER)",
            "CREATE INDEX messages_project ON messages(project_id)",
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE project_mailbox_deliveries (message_id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
            "CREATE TABLE project_mailbox_receipts (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, expires_ts INTEGER)",
            "INSERT INTO projects VALUES (1, 'alpha'), (2, 'empty')",
        ] {
            execute(&conn, sql);
        }
        (dir, conn)
    }

    fn seed(conn: &DbConn, viewers: usize, messages: usize) {
        execute(conn, "BEGIN");
        for start in (1..=viewers).step_by(OVERVIEW_PAGE_ROWS) {
            let rows: Vec<_> = (start..=(start + OVERVIEW_PAGE_ROWS - 1).min(viewers))
                .map(|id| format!("({id},1,0,NULL,'auto')"))
                .collect();
            execute(
                conn,
                &format!("INSERT INTO agents VALUES {}", rows.join(",")),
            );
        }
        for start in (1..=messages).step_by(OVERVIEW_PAGE_ROWS) {
            let end = (start + OVERVIEW_PAGE_ROWS - 1).min(messages);
            let rows: Vec<_> = (start..=end)
                .map(|id| format!("({id},1,1,{SENT},'high',1)"))
                .collect();
            execute(
                conn,
                &format!("INSERT INTO messages VALUES {}", rows.join(",")),
            );
            let deliveries: Vec<_> = (start..=end).map(|id| format!("({id},1)")).collect();
            execute(
                conn,
                &format!(
                    "INSERT INTO project_mailbox_deliveries VALUES {}",
                    deliveries.join(",")
                ),
            );
        }
        execute(conn, "COMMIT");
    }

    // Exercise the actual DB-layer inbox-policy query, including its original
    // message-by-viewer expansion. Do not replace the incumbent with a stub.
    fn canonical(conn: &DbConn) -> serde_json::Value {
        let mut projects = HashMap::new();
        let mut after = None;
        loop {
            let (predicate, params) = after.map_or_else(
                || ("", Vec::new()),
                |id| ("WHERE message_id > ?", vec![Value::BigInt(id)]),
            );
            let page: Vec<_> = conn
                .query_sync(
                    &format!(
                        "SELECT message_id AS id FROM project_mailbox_deliveries \
                 {predicate} ORDER BY message_id LIMIT {OVERVIEW_PAGE_ROWS}"
                    ),
                    &params,
                )
                .unwrap()
                .iter()
                .map(|row| integer(row, "id").unwrap())
                .collect();
            if page.is_empty() {
                break;
            }
            let (sql, params) = mcp_agent_mail_db::project_mailbox::overview_counts_query(
                &page,
                micros_ago(NOW, ACK_OVERDUE_THRESHOLD_US),
            )
            .unwrap();
            for row in conn.query_sync(&sql, &params).unwrap() {
                let count = project(&mut projects, integer(&row, "project_id").unwrap());
                count.unread += usize::try_from(integer(&row, "unread").unwrap()).unwrap();
                count.urgent += usize::try_from(integer(&row, "urgent").unwrap()).unwrap();
                count.ack_overdue +=
                    usize::try_from(integer(&row, "ack_overdue").unwrap()).unwrap();
            }
            if page.len() < OVERVIEW_PAGE_ROWS {
                break;
            }
            after = page.last().copied();
        }
        serde_json::to_value(projects).unwrap()
    }

    fn optimized(conn: &DbConn) -> (serde_json::Value, ScanWork) {
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        collect_project_mailboxes(conn, NOW, &mut projects, &mut work).unwrap();
        assert!(work.peak_query_rows <= OVERVIEW_PAGE_ROWS);
        (serde_json::to_value(projects).unwrap(), work)
    }

    fn first_page(conn: &DbConn) -> Vec<Row> {
        conn.query_sync(&format!(
            "SELECT message_id AS id, project_id FROM project_mailbox_deliveries ORDER BY message_id LIMIT {OVERVIEW_PAGE_ROWS}"
        ), &[]).unwrap()
    }

    #[test]
    fn shared_fanout_counts_population_without_virtual_recipient_rows() {
        let (_dir, conn) = fixture();
        seed(&conn, 200, OVERVIEW_PAGE_ROWS * 2 + 1);
        let expected = canonical(&conn);
        let (actual, work) = optimized(&conn);
        assert_eq!(actual, expected);
        assert_eq!(actual["1"]["unread"], 199 * (OVERVIEW_PAGE_ROWS * 2 + 1));
        assert_eq!(
            work.project_mailbox_message_rows,
            OVERVIEW_PAGE_ROWS * 2 + 1
        );
        assert!(
            conn.query_sync("SELECT * FROM message_recipients", &[])
                .unwrap()
                .is_empty()
        );
        assert!(
            conn.query_sync("SELECT * FROM project_mailbox_receipts", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn shared_policy_and_receipt_corrections_match_the_canonical_visibility_query() {
        let (_dir, conn) = fixture();
        seed(&conn, 12, 3);
        execute(
            &conn,
            "UPDATE agents SET contact_policy='BlOcK_AlL' WHERE id=3",
        );
        execute(&conn, "UPDATE agents SET retired_at=0 WHERE id=4");
        execute(
            &conn,
            &format!("UPDATE agents SET inception_ts={} WHERE id=5", SENT + 1),
        );
        execute(
            &conn,
            &format!("UPDATE agents SET inception_ts={SENT} WHERE id=6"),
        );
        execute(&conn, "UPDATE agents SET inception_ts=NULL WHERE id=7");
        execute(&conn, "UPDATE agents SET project_id=2 WHERE id=8");
        execute(&conn, "UPDATE agents SET contact_policy=NULL WHERE id=9");
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES (1,2,NULL,NULL), (2,2,0,0)",
        );
        execute(
            &conn,
            "INSERT INTO project_mailbox_receipts VALUES
            (1,1,0,0), (1,2,0,0), (1,3,0,0), (1,4,0,0), (1,5,0,0),
            (1,8,0,0), (1,999,0,0), (1,9,0,NULL), (1,10,NULL,0),
            (2,9,NULL,NULL), (2,10,0,0)",
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
        let (full, _) = build_at(&conn, NOW).unwrap();
        let counts: serde_json::Value =
            serde_json::from_str(&build_counts_output(&conn, OutputFormat::Json).unwrap()).unwrap();
        assert_eq!(counts["project_count"], full.len());
        let alpha = full.iter().find(|row| row.slug == "alpha").unwrap();
        // The direct unread row is counted once by the direct phase, not again
        // by the virtual delivery despite an independent shared receipt.
        let shared_unread =
            usize::try_from(optimized(&conn).0["1"]["unread"].as_u64().unwrap()).unwrap();
        assert_eq!(alpha.unread, shared_unread + 1);
    }

    #[test]
    fn strict_send_and_overdue_boundaries_orphans_and_dangling_rows_are_preserved() {
        let (_dir, conn) = fixture();
        seed(&conn, 2, 3);
        execute(
            &conn,
            &format!(
                "UPDATE messages SET created_ts={} WHERE id=1",
                NOW - ACK_OVERDUE_THRESHOLD_US
            ),
        );
        execute(
            &conn,
            "UPDATE messages SET importance='URGENT', ack_required=2 WHERE id=2",
        );
        execute(
            &conn,
            "UPDATE project_mailbox_deliveries SET project_id=77 WHERE message_id=3",
        );
        execute(
            &conn,
            "INSERT INTO project_mailbox_deliveries VALUES (999,1)",
        );
        execute(&conn, "INSERT INTO agents VALUES (99,88,0,NULL,'auto')");
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES ({},88,999,{SENT},'urgent',1)",
                i64::MIN
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO project_mailbox_deliveries VALUES ({},88)",
                i64::MIN
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES ({},1,1,{SENT},'urgent',1)",
                i64::MAX
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO project_mailbox_deliveries VALUES ({},1)",
                i64::MAX
            ),
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(optimized(&conn).0["88"]["ack_overdue"], 1);
    }

    #[test]
    fn dense_corrections_fall_back_without_partial_counts_or_truncating_later_pages() {
        for direct in [false, true] {
            let (_dir, conn) = fixture();
            seed(&conn, 3, OVERVIEW_PAGE_ROWS * 2 + 1);
            execute(&conn, "BEGIN");
            for id in 1..=OVERVIEW_PAGE_ROWS {
                let sql = if direct {
                    format!("INSERT INTO message_recipients VALUES ({id},2,0,0)")
                } else {
                    format!("INSERT INTO project_mailbox_receipts VALUES ({id},2,0,0)")
                };
                execute(&conn, &sql);
            }
            execute(&conn, "COMMIT");
            let mut collector = Collector::default();
            let mut projects = HashMap::new();
            let mut work = ScanWork::default();
            assert!(
                !collector
                    .try_page(
                        &conn,
                        &first_page(&conn),
                        SENT + 1,
                        &mut projects,
                        &mut work
                    )
                    .unwrap()
            );
            assert!(projects.is_empty());
            let queries = work.queries;
            assert!(
                !collector
                    .try_page(
                        &conn,
                        &first_page(&conn),
                        SENT + 1,
                        &mut projects,
                        &mut work
                    )
                    .unwrap()
            );
            assert_eq!(
                work.queries, queries,
                "dense fallback must not repeat probes"
            );
            assert_eq!(optimized(&conn).0, canonical(&conn));
        }
    }

    #[test]
    fn viewer_cap_falls_back_instead_of_omitting_an_obligation() {
        let (_dir, conn) = fixture();
        seed(&conn, MAX_SHARED_VIEWERS + 1, 1);
        let mut collector = Collector::default();
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        assert!(
            !collector
                .try_page(
                    &conn,
                    &first_page(&conn),
                    SENT + 1,
                    &mut projects,
                    &mut work
                )
                .unwrap()
        );
        assert!(projects.is_empty());
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(optimized(&conn).0["1"]["unread"], MAX_SHARED_VIEWERS);
    }

    #[test]
    fn nonunique_or_partial_pair_indexes_cannot_authorize_population_subtraction() {
        for ddl in [
            "CREATE INDEX pair ON exceptions(message_id,agent_id)",
            "CREATE UNIQUE INDEX pair ON exceptions(message_id,agent_id) WHERE message_id > 0",
            "CREATE UNIQUE INDEX pair ON exceptions(message_id,agent_id,other)",
        ] {
            let (_dir, conn) = fixture();
            execute(
                &conn,
                "CREATE TABLE exceptions (message_id INTEGER, agent_id INTEGER, other INTEGER)",
            );
            execute(&conn, ddl);
            assert!(pair_index(&conn, "exceptions").unwrap().is_none());
        }
    }

    #[test]
    fn shared_counts_work_read_only_preserve_outer_transaction_and_release_errors() {
        let (_dir, conn) = fixture();
        seed(&conn, 3, 2);
        execute(&conn, "BEGIN");
        execute(
            &conn,
            "UPDATE agents SET contact_policy='block_all' WHERE id=2",
        );
        let inside = build_at(&conn, NOW).unwrap().0;
        assert_eq!(
            inside
                .iter()
                .find(|row| row.slug == "alpha")
                .unwrap()
                .unread,
            2
        );
        execute(&conn, "ROLLBACK");
        execute(&conn, "PRAGMA query_only=ON");
        assert_eq!(optimized(&conn).0, canonical(&conn));
        execute(&conn, "PRAGMA query_only=OFF");
        execute(
            &conn,
            "ALTER TABLE project_mailbox_receipts RENAME COLUMN read_ts TO broken",
        );
        assert!(build_at(&conn, NOW).is_err());
        assert!(
            conn.execute_sync("RELEASE robot_overview_read", &[])
                .is_err()
        );
        execute(
            &conn,
            "ALTER TABLE project_mailbox_receipts RENAME COLUMN broken TO read_ts",
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
    }

    #[test]
    fn partial_shared_schema_is_not_an_empty_mailbox_even_without_delivery_rows() {
        for absent in ["project_mailbox_deliveries", "project_mailbox_receipts"] {
            let (_dir, conn) = fixture();
            execute(&conn, &format!("DROP TABLE {absent}"));
            assert!(build_at(&conn, NOW).is_err());
            assert!(
                conn.execute_sync("RELEASE robot_overview_read", &[])
                    .is_err()
            );
        }
    }

    #[test]
    fn nullable_legacy_pairs_never_settle_an_agents_obligation() {
        let (_dir, conn) = fixture();
        seed(&conn, 3, 1);
        // SQLite UNIQUE pairs can contain duplicate NULLs. The canonical
        // viewer join excludes them; they cannot become receipt corrections.
        execute(
            &conn,
            "INSERT INTO message_recipients VALUES
            (1,NULL,NULL,NULL), (1,NULL,0,0)",
        );
        execute(
            &conn,
            "INSERT INTO project_mailbox_receipts VALUES
            (1,NULL,0,0), (1,NULL,0,NULL), (NULL,2,0,0)",
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(optimized(&conn).0["1"]["unread"], 2);
        let mut projects = HashMap::new();
        let mut work = ScanWork::default();
        assert!(
            Collector::default()
                .try_page(
                    &conn,
                    &first_page(&conn),
                    SENT + 1,
                    &mut projects,
                    &mut work,
                )
                .unwrap()
        );
    }

    #[test]
    fn population_and_receipts_stay_in_one_wal_snapshot_and_refresh_next_time() {
        let (dir, reader) = fixture();
        execute(&reader, "PRAGMA journal_mode=WAL");
        seed(&reader, 3, 1);
        let expected = canonical(&reader);
        let writer =
            DbConn::open_file(dir.path().join("shared.sqlite3").to_str().unwrap()).unwrap();
        execute(&writer, "PRAGMA busy_timeout=1000");
        execute(&reader, "SAVEPOINT shared_test");
        let mut work = ScanWork::default();
        let state = Collector::prepare(&reader, &mut work).unwrap();
        execute(
            &writer,
            "INSERT INTO project_mailbox_receipts VALUES (1,2,0,0)",
        );
        execute(
            &writer,
            "UPDATE agents SET contact_policy='block_all' WHERE id=3",
        );
        let mut collector = Collector { state };
        let mut projects = HashMap::new();
        let result = collector.try_page(
            &reader,
            &first_page(&reader),
            SENT + 1,
            &mut projects,
            &mut work,
        );
        execute(&reader, "RELEASE shared_test");
        assert!(result.unwrap());
        assert_eq!(serde_json::to_value(projects).unwrap(), expected);
        assert_eq!(optimized(&reader).0, canonical(&reader));
        assert!(optimized(&reader).0.as_object().unwrap().is_empty());
    }

    #[test]
    #[ignore = "native shared-mailbox phase comparison; not end-to-end CLI latency"]
    fn benchmark_shared_population_against_canonical_fanout() {
        let (_dir, conn) = fixture();
        seed(&conn, 200, 1024);
        let mut times = [Vec::new(), Vec::new()];
        let expected = canonical(&conn);
        for iteration in 0..6 {
            for optimized_first in [iteration % 2 == 0, iteration % 2 != 0] {
                execute(&conn, "SAVEPOINT shared_benchmark");
                let started = Instant::now();
                let actual = if optimized_first {
                    optimized(&conn).0
                } else {
                    canonical(&conn)
                };
                let elapsed = started.elapsed();
                execute(&conn, "RELEASE shared_benchmark");
                assert_eq!(actual, expected);
                times[usize::from(optimized_first)].push(elapsed);
            }
        }
        times.iter_mut().for_each(|values| values.sort_unstable());
        eprintln!(
            "native shared phase, 1024 deliveries, 200 agents: canonical={:?}, population={:?}",
            times[0][3], times[1][3]
        );
    }

    fn seed_unrelated(conn: &DbConn) {
        execute(conn, "BEGIN");
        for start in (1..=MAX_SHARED_VIEWERS + 1).step_by(OVERVIEW_PAGE_ROWS) {
            let end = (start + OVERVIEW_PAGE_ROWS - 1).min(MAX_SHARED_VIEWERS + 1);
            let rows: Vec<_> = (start..=end)
                .map(|id| format!("({},999,0,NULL,'auto')", 100_000 + id))
                .collect();
            execute(
                conn,
                &format!("INSERT INTO agents VALUES {}", rows.join(",")),
            );
        }
        execute(conn, "COMMIT");
    }

    fn delivery_page(conn: &DbConn, first: i64, last: i64) -> Vec<Row> {
        conn.query_sync(&format!(
            "SELECT message_id AS id, project_id FROM project_mailbox_deliveries \
             WHERE message_id >= ? AND message_id <= ? ORDER BY message_id LIMIT {OVERVIEW_PAGE_ROWS}"
        ), &[Value::BigInt(first), Value::BigInt(last)]).unwrap()
    }

    #[test]
    fn unrelated_fleet_overflow_recovers_a_complete_bounded_project_population() {
        let (_dir, conn) = fixture();
        seed(&conn, 20, OVERVIEW_PAGE_ROWS * 2 + 1);
        seed_unrelated(&conn);
        let mut collector = Collector::default();
        let mut counts = HashMap::new();
        let mut work = ScanWork::default();
        assert!(
            collector
                .try_page(&conn, &first_page(&conn), SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        let State::Scoped(scoped) = &collector.state else {
            panic!("expected scoped recovery")
        };
        assert_eq!(scoped.population.as_ref().unwrap().viewers.len(), 20);
        assert_eq!(
            scoped.attempted_scope.as_ref().unwrap(),
            &HashSet::from([1])
        );
        assert_eq!(scoped.loads_left, MAX_SCOPED_POPULATION_LOADS - 1);
        let queries = work.queries;
        let page = delivery_page(&conn, 257, 512);
        assert!(
            collector
                .try_page(&conn, &page, SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        assert_eq!(
            work.queries - queries,
            3,
            "reuse scope; only correction and message queries"
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(optimized(&conn).0["1"]["unread"], 19 * 513);
    }

    #[test]
    fn an_oversized_project_does_not_disable_later_small_or_empty_project_scopes() {
        let (_dir, conn) = fixture();
        seed(&conn, MAX_SHARED_VIEWERS + 1, 1);
        execute(&conn, "INSERT INTO agents VALUES (10000,77,0,NULL,'auto')");
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES (2,77,999,{SENT},'high',1), (3,88,999,{SENT},'high',1)"
            ),
        );
        execute(
            &conn,
            "INSERT INTO project_mailbox_deliveries VALUES (2,77),(3,88)",
        );
        let mut work = ScanWork::default();
        let mut collector = Collector {
            state: Collector::prepare(&conn, &mut work).unwrap(),
        };
        let mut counts = HashMap::new();
        let queries = work.queries;
        assert!(
            !collector
                .try_page(
                    &conn,
                    &delivery_page(&conn, 1, 1),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert_eq!(
            work.queries, queries,
            "do not reload a proven oversized project"
        );
        assert!(counts.is_empty());
        assert!(
            collector
                .try_page(
                    &conn,
                    &delivery_page(&conn, 2, 2),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert_eq!(counts[&77].unread, 1);
        assert!(
            collector
                .try_page(
                    &conn,
                    &delivery_page(&conn, 3, 3),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert!(!counts.contains_key(&88));
        let State::Scoped(scoped) = &collector.state else {
            panic!("scope stays available")
        };
        assert!(scoped.population.as_ref().unwrap().viewers.is_empty());
        let queries = work.queries;
        assert!(
            collector
                .try_page(
                    &conn,
                    &delivery_page(&conn, 3, 3),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert_eq!(
            work.queries - queries,
            3,
            "an empty scope is a complete cached population"
        );
    }

    #[test]
    fn failed_combined_scope_does_not_poison_a_smaller_complete_scope() {
        let (_dir, conn) = fixture();
        seed(&conn, MAX_SHARED_VIEWERS + 2, 2);
        execute(&conn, "UPDATE agents SET project_id=2 WHERE id > 2049");
        execute(&conn, "UPDATE messages SET project_id=2 WHERE id=2");
        execute(
            &conn,
            "UPDATE project_mailbox_deliveries SET project_id=2 WHERE message_id=2",
        );
        let mut collector = Collector::default();
        let mut counts = HashMap::new();
        let mut work = ScanWork::default();
        let both = first_page(&conn);
        assert!(
            !collector
                .try_page(&conn, &both, SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        assert!(counts.is_empty(), "no partial scope can publish counters");
        let queries = work.queries;
        assert!(
            !collector
                .try_page(&conn, &both, SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        assert_eq!(
            work.queries, queries,
            "do not retry the identical over-cap scope"
        );
        assert!(
            collector
                .try_page(
                    &conn,
                    &delivery_page(&conn, 1, 1),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert_eq!(counts[&1].unread, 2048);
        let State::Scoped(scoped) = &collector.state else {
            panic!("scoped state")
        };
        assert_eq!(scoped.population.as_ref().unwrap().viewers.len(), 2049);
    }

    #[test]
    fn scope_budget_exhaustion_keeps_late_obligations_via_the_canonical_fallback() {
        let (_dir, conn) = fixture();
        let n = MAX_SCOPED_POPULATION_LOADS + 2;
        seed(&conn, n, n * OVERVIEW_PAGE_ROWS);
        execute(&conn, "UPDATE agents SET project_id=id");
        execute(
            &conn,
            &format!("UPDATE messages SET project_id=(id-1)/{OVERVIEW_PAGE_ROWS}+1"),
        );
        execute(
            &conn,
            &format!(
                "UPDATE project_mailbox_deliveries SET project_id=(message_id-1)/{OVERVIEW_PAGE_ROWS}+1"
            ),
        );
        seed_unrelated(&conn);
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(
            optimized(&conn).0[&n.to_string()]["ack_overdue"],
            OVERVIEW_PAGE_ROWS
        );
        let mut collector = Collector::default();
        let mut counts = HashMap::new();
        let mut work = ScanWork::default();
        for id in 1..=n {
            let first = i64::try_from((id - 1) * OVERVIEW_PAGE_ROWS + 1).unwrap();
            let page = delivery_page(&conn, first, first);
            let before = work.queries;
            let used = collector
                .try_page(&conn, &page, SENT + 1, &mut counts, &mut work)
                .unwrap();
            assert_eq!(used, id <= MAX_SCOPED_POPULATION_LOADS);
            if !used {
                assert_eq!(work.queries, before);
            }
        }
    }

    #[test]
    fn unsuitable_agent_indexes_do_not_enable_scoped_history_scans() {
        for ddl in [
            None,
            Some("CREATE INDEX bad ON agents(project_id) WHERE project_id=1"),
            Some("CREATE INDEX bad ON agents(inception_ts,project_id)"),
            Some("CREATE INDEX bad ON agents((project_id+0))"),
        ] {
            let (_dir, conn) = fixture();
            seed(&conn, 3, 1);
            seed_unrelated(&conn);
            execute(&conn, "DROP INDEX agents_project");
            if let Some(ddl) = ddl {
                execute(&conn, ddl);
            }
            let mut work = ScanWork::default();
            assert!(matches!(
                Collector::prepare(&conn, &mut work).unwrap(),
                State::Disabled
            ));
            assert_eq!(optimized(&conn).0, canonical(&conn));
        }
    }

    #[test]
    fn scoped_cursor_handles_signed_ids_and_a_quoted_composite_index() {
        let (_dir, conn) = fixture();
        seed_unrelated(&conn);
        execute(&conn, "DROP INDEX agents_project");
        execute(
            &conn,
            "CREATE INDEX \"scope\"\"order\" ON agents(project_id DESC,inception_ts DESC)",
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO agents VALUES ({},{},0,NULL,'auto'), ({},{},1,NULL,'auto')",
                i64::MIN,
                i64::MIN,
                i64::MAX,
                i64::MIN
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO messages VALUES (1,{},999,{SENT},'high',1)",
                i64::MIN
            ),
        );
        execute(
            &conn,
            &format!(
                "INSERT INTO project_mailbox_deliveries VALUES (1,{})",
                i64::MIN
            ),
        );
        assert_eq!(optimized(&conn).0, canonical(&conn));
        assert_eq!(optimized(&conn).0[&i64::MIN.to_string()]["unread"], 2);
    }

    #[test]
    fn dense_scoped_corrections_disable_repeated_probes_without_publishing_partial_counts() {
        let (_dir, conn) = fixture();
        seed(&conn, 3, OVERVIEW_PAGE_ROWS + 1);
        seed_unrelated(&conn);
        execute(
            &conn,
            "INSERT INTO project_mailbox_receipts SELECT message_id,2,0,0 FROM project_mailbox_deliveries",
        );
        let mut collector = Collector::default();
        let mut counts = HashMap::new();
        let mut work = ScanWork::default();
        assert!(
            !collector
                .try_page(&conn, &first_page(&conn), SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        assert!(counts.is_empty());
        assert!(matches!(collector.state, State::Disabled));
        let queries = work.queries;
        assert!(
            !collector
                .try_page(&conn, &first_page(&conn), SENT + 1, &mut counts, &mut work)
                .unwrap()
        );
        assert_eq!(work.queries, queries);
        assert_eq!(optimized(&conn).0, canonical(&conn));
    }

    #[test]
    fn scoped_load_and_receipts_keep_the_same_wal_snapshot() {
        let (dir, reader) = fixture();
        seed(&reader, 3, 1);
        seed_unrelated(&reader);
        execute(&reader, "PRAGMA journal_mode=WAL");
        let expected = canonical(&reader);
        let writer =
            DbConn::open_file(dir.path().join("shared.sqlite3").to_str().unwrap()).unwrap();
        execute(&reader, "SAVEPOINT scoped_snapshot");
        let mut work = ScanWork::default();
        let state = Collector::prepare(&reader, &mut work).unwrap();
        assert!(matches!(&state, State::Scoped(_)));
        execute(
            &writer,
            "INSERT INTO project_mailbox_receipts VALUES (1,2,0,0)",
        );
        execute(
            &writer,
            "UPDATE agents SET contact_policy='block_all' WHERE id=3",
        );
        let mut collector = Collector { state };
        let mut counts = HashMap::new();
        assert!(
            collector
                .try_page(
                    &reader,
                    &first_page(&reader),
                    SENT + 1,
                    &mut counts,
                    &mut work
                )
                .unwrap()
        );
        assert_eq!(serde_json::to_value(counts).unwrap(), expected);
        execute(&reader, "RELEASE scoped_snapshot");
        assert_eq!(optimized(&reader).0, canonical(&reader));
        assert!(optimized(&reader).0.as_object().unwrap().is_empty());
    }

    #[test]
    fn scoped_query_failure_is_an_error_not_an_empty_population() {
        let (_dir, conn) = fixture();
        seed(&conn, 3, 1);
        seed_unrelated(&conn);
        let mut work = ScanWork::default();
        let mut collector = Collector {
            state: Collector::prepare(&conn, &mut work).unwrap(),
        };
        assert!(matches!(&collector.state, State::Scoped(_)));
        // Simulate a refused verified-index query. Production holds the schema
        // stable in a snapshot; even an unexpected engine refusal must not be
        // converted into successful zero counts or an incomplete population.
        execute(&conn, "DROP INDEX agents_project");
        let mut counts = HashMap::new();
        assert!(
            collector
                .try_page(&conn, &first_page(&conn), SENT + 1, &mut counts, &mut work)
                .is_err()
        );
        assert!(counts.is_empty());
    }

    // Frozen population preparation from 5e178010, before scoped recovery.
    // Keep its actual SQL and cap behavior in the same native invocation.
    fn incumbent_state(conn: &DbConn, work: &mut ScanWork) -> State {
        let Some(direct_index) = pair_index(conn, "message_recipients").unwrap() else {
            return State::Disabled;
        };
        let Some(receipt_index) = pair_index(conn, "project_mailbox_receipts").unwrap() else {
            return State::Disabled;
        };
        let mut viewers = HashMap::new();
        let mut inception: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut after = None;
        loop {
            let (continuation, params) = after.map_or_else(
                || ("", Vec::new()),
                |id| (" AND id > ?", vec![Value::BigInt(id)]),
            );
            let rows = bounded_query(conn,&format!(
                "{VIEWERS_SQL} WHERE ({VIEWERS_FILTER}){continuation} ORDER BY id LIMIT {OVERVIEW_PAGE_ROWS}"
            ),&params,work).unwrap();
            if viewers.len() + rows.len() > MAX_SHARED_VIEWERS {
                return State::Disabled;
            }
            for row in &rows {
                let id = integer(row, "id").unwrap();
                assert!(after.is_none_or(|previous| id > previous));
                after = Some(id);
                let viewer = Viewer {
                    project_id: integer(row, "project_id").unwrap(),
                    inception_ts: integer(row, "inception_ts").unwrap(),
                };
                inception
                    .entry(viewer.project_id)
                    .or_default()
                    .push(viewer.inception_ts);
                viewers.insert(id, viewer);
            }
            if rows.len() < OVERVIEW_PAGE_ROWS {
                break;
            }
        }
        for times in inception.values_mut() {
            times.sort_unstable();
        }
        State::Ready(Population {
            viewers,
            inception,
            direct_index,
            receipt_index,
        })
    }

    fn measured_shared_phase(conn: &DbConn, recover: bool) -> (serde_json::Value, ScanWork) {
        let mut collector = Collector::default();
        let mut counts = HashMap::new();
        let mut work = ScanWork::default();
        let mut after = None;
        let mut prepared = false;
        loop {
            let (predicate, params) = after.map_or_else(
                || ("", Vec::new()),
                |id| ("WHERE message_id > ?", vec![Value::BigInt(id)]),
            );
            let rows = bounded_query(
                conn,
                &format!(
                    "SELECT message_id AS id, project_id FROM project_mailbox_deliveries \
                 {predicate} ORDER BY message_id LIMIT {OVERVIEW_PAGE_ROWS}"
                ),
                &params,
                &mut work,
            )
            .unwrap();
            if rows.is_empty() {
                break;
            }
            if !recover && !prepared {
                collector.state = incumbent_state(conn, &mut work);
                prepared = true;
            }
            if !collector
                .try_page(conn, &rows, SENT + 1, &mut counts, &mut work)
                .unwrap()
            {
                let ids: Vec<_> = rows.iter().map(|row| integer(row, "id").unwrap()).collect();
                let (sql, params) =
                    mcp_agent_mail_db::project_mailbox::overview_counts_query(&ids, SENT + 1)
                        .unwrap();
                for row in bounded_query(conn, &sql, &params, &mut work).unwrap() {
                    let count = project(&mut counts, integer(&row, "project_id").unwrap());
                    count.unread += usize::try_from(integer(&row, "unread").unwrap()).unwrap();
                    count.urgent += usize::try_from(integer(&row, "urgent").unwrap()).unwrap();
                    count.ack_overdue +=
                        usize::try_from(integer(&row, "ack_overdue").unwrap()).unwrap();
                }
            }
            if rows.len() < OVERVIEW_PAGE_ROWS {
                break;
            }
            after = Some(integer(rows.last().unwrap(), "id").unwrap());
        }
        (serde_json::to_value(counts).unwrap(), work)
    }

    #[test]
    #[ignore = "native incumbent/scoped shared-phase benchmark including losing cases; not CLI startup"]
    fn benchmark_scoped_population_against_global_cap_fallback() {
        for (name, viewers, deliveries, noise, dense) in [
            ("small-fleet", 200, 513, false, false),
            ("unrelated-fleet", 200, 513, true, false),
            ("dense-fleet", 200, 513, true, true),
            ("oversized-project", MAX_SHARED_VIEWERS + 1, 1, false, false),
        ] {
            let (_dir, conn) = fixture();
            seed(&conn, viewers, deliveries);
            if noise {
                seed_unrelated(&conn);
            }
            if dense {
                execute(
                    &conn,
                    "INSERT INTO project_mailbox_receipts \
                    SELECT d.message_id, a.id, 0, 0 FROM project_mailbox_deliveries d \
                    JOIN agents a ON a.project_id=d.project_id WHERE a.id <> 1",
                );
            }
            let expected = canonical(&conn);
            let mut times = [Vec::new(), Vec::new()];
            for iteration in 0..6 {
                for recover in [iteration % 2 == 0, iteration % 2 != 0] {
                    execute(&conn, "SAVEPOINT scoped_benchmark");
                    let started = Instant::now();
                    let (actual, work) = measured_shared_phase(&conn, recover);
                    let elapsed = started.elapsed();
                    execute(&conn, "RELEASE scoped_benchmark");
                    assert_eq!(actual, expected, "{name} recover={recover}");
                    assert!(work.peak_query_rows <= OVERVIEW_PAGE_ROWS);
                    eprintln!("{name} recover={recover} elapsed={elapsed:?} work={work:?}");
                    times[usize::from(recover)].push(elapsed);
                }
            }
            times.iter_mut().for_each(|values| values.sort_unstable());
            eprintln!(
                "native shared phase {name}: incumbent={:?}, scoped={:?}",
                times[0][3], times[1][3]
            );
        }
    }
}
