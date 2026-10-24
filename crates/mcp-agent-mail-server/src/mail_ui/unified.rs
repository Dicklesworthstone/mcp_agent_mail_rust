//! Message-first projection for the operator's HTML/JSON unified inbox.
//!
//! The roster remains the one selected by the existing project/agent readers.
//! Receipt and body reads share one checkout and snapshot. Read newest-first
//! header pages until the visible window is full, then hydrate only that window.
//! Sparse filters can still scan history, without retaining history-sized bodies.
//! No cache, receipt write, or replacement pool is used.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asupersync::{CancelReason, Cx, Outcome};
use mcp_agent_mail_db::DbError;
use mcp_agent_mail_db::mail_explorer::ExplorerQuery;
use mcp_agent_mail_db::models::{MessageRow, ProjectRow};
use mcp_agent_mail_db::pool::{DbPool, SyncQuery};
use mcp_agent_mail_db::queries::{self, InboxRow, UNKNOWN_SENDER_DISPLAY};
use mcp_agent_mail_db::sqlmodel_core::{Error as SqlError, Row, Value};

use super::{
    UnifiedMessageAggregate, block_on_outcome, explicit_thread_ref, matches_importance_filter,
};

const PAGE_SIZE: usize = 256;
const SAVEPOINT: &str = "SAVEPOINT am_unified_inbox_read";
const RELEASE: &str = "RELEASE SAVEPOINT am_unified_inbox_read";
// Both ordering terms let the runtime use a reverse timestamp-index range seek.
// The first page's inclusive bound retains even the largest valid timestamp.
const FIRST_HEADERS: &str = "SELECT id, project_id, sender_id, created_ts, importance FROM messages WHERE created_ts <= 9223372036854775807 ORDER BY created_ts DESC, id DESC LIMIT 256";
const NEXT_HEADERS: &str = "SELECT id, project_id, sender_id, created_ts, importance FROM messages WHERE created_ts <= ? ORDER BY created_ts DESC, id DESC LIMIT 256 OFFSET ?";

type Roster = BTreeMap<i64, BTreeSet<i64>>;

fn database_error(error: impl std::fmt::Display) -> DbError {
    let detail = error.to_string();
    if mcp_agent_mail_db::is_lock_error(&detail) {
        DbError::ResourceBusy(detail)
    } else {
        DbError::Sqlite(detail)
    }
}

fn cancelled<T>(cx: &Cx) -> Outcome<T, DbError> {
    Outcome::Cancelled(
        cx.cancel_reason()
            .unwrap_or_else(|| CancelReason::user("unified inbox read cancelled")),
    )
}

// Generic over the acquired checkout so the server need not add a direct
// dependency on the pool implementation. An unsuccessful read never returns
// an unfinished savepoint to that pool, including during unwinding.
struct ReadCheckout<C, F: FnOnce(C)> {
    connection: Option<C>,
    retire: Option<F>,
}

impl<C, F: FnOnce(C)> ReadCheckout<C, F> {
    fn new(connection: C, retire: F) -> Self {
        Self {
            connection: Some(connection),
            retire: Some(retire),
        }
    }
}

impl<C, F: FnOnce(C)> Drop for ReadCheckout<C, F> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Some(retire) = self.retire.take() {
                retire(connection);
            } else {
                drop(connection);
            }
        }
    }
}

struct CheckedRead<'a, C> {
    connection: &'a C,
    cx: &'a Cx,
    interrupted: std::cell::Cell<bool>,
}

impl<C: SyncQuery> CheckedRead<'_, C> {
    fn checkpoint(&self) -> Result<(), SqlError> {
        if self.cx.checkpoint().is_err() {
            self.interrupted.set(true);
            Err(SqlError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl<C: SyncQuery> SyncQuery for CheckedRead<'_, C> {
    fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
        self.checkpoint()?;
        let started = mcp_agent_mail_db::query_timer();
        let rows = self.connection.query_sync(sql, params);
        mcp_agent_mail_db::record_query(sql, mcp_agent_mail_db::elapsed_us(started));
        let rows = rows?;
        self.checkpoint()?;
        Ok(rows)
    }

    fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
        if sql != RELEASE {
            self.checkpoint()?;
        }
        self.connection.execute_raw(sql)
    }
}

struct Snapshot<'a, C: SyncQuery> {
    connection: &'a C,
    active: bool,
}

impl<C: SyncQuery> Drop for Snapshot<'_, C> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.connection.execute_raw(RELEASE);
        }
    }
}

fn in_snapshot<C: SyncQuery, T>(
    connection: &C,
    read: impl FnOnce() -> Result<T, DbError>,
) -> Result<T, DbError> {
    connection.execute_raw(SAVEPOINT).map_err(database_error)?;
    let mut snapshot = Snapshot {
        connection,
        active: true,
    };
    let result = read();
    connection.execute_raw(RELEASE).map_err(database_error)?;
    snapshot.active = false;
    result
}

#[derive(Debug)]
struct Viewer {
    name: String,
    inception: i64,
    retired: bool,
    blocked: bool,
}

#[derive(Debug)]
struct Header {
    id: i64,
    project: i64,
    sender: i64,
    created: i64,
    importance: String,
}

#[derive(Debug)]
struct Receipt {
    name: String,
    kind: String,
    shared: bool,
    read: Option<i64>,
    ack: Option<i64>,
}

struct Candidate {
    header: Header,
    recipients: BTreeMap<i64, Receipt>,
}

struct SelectedMessage {
    inbox: InboxRow,
    recipients: Vec<Receipt>,
}

pub(super) fn collect(
    cx: &Cx,
    pool: &DbPool,
    projects: &[ProjectRow],
    limit: usize,
    importance: Option<&str>,
) -> Result<Vec<UnifiedMessageAggregate>, (u16, String)> {
    if limit == 0 || projects.is_empty() {
        return Ok(Vec::new());
    }
    // Preserve the existing list_agents visibility contract, including legacy
    // identities. Recheck these exact IDs in the read snapshot; do not discover
    // additional viewers while expanding shared deliveries.
    let mut roster = Roster::new();
    for project in projects {
        let project_id = project.id.unwrap_or(0);
        let agents = block_on_outcome(cx, queries::list_agents(cx, pool, project_id))?;
        roster
            .entry(project_id)
            .or_default()
            .extend(agents.into_iter().map(|agent| agent.id.unwrap_or(0)));
    }
    let selected = block_on_outcome(cx, fetch(cx, pool, &roster, limit, importance))?;
    let projects: HashMap<_, _> = projects
        .iter()
        .map(|project| (project.id.unwrap_or(0), project))
        .collect();
    // Keep the established numeric-thread convention. These existing bounded
    // root probes run after releasing the checkout (also safe in a size-1 pool).
    // A newly arrived reply may add a link, but cannot change page membership,
    // recipients, read status or body from the completed message snapshot.
    let mut root_ids: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for selected in &selected {
        if explicit_thread_ref(selected.inbox.message.thread_id.as_deref()).is_none()
            && let Some(id) = selected.inbox.message.id.filter(|id| *id > 0)
        {
            root_ids
                .entry(selected.inbox.message.project_id)
                .or_default()
                .push(id);
        }
    }
    let mut roots = HashMap::new();
    for (project, ids) in root_ids {
        roots.insert(
            project,
            super::root_ids_with_replies(cx, pool, project, &ids)?,
        );
    }
    let empty_roots = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(selected.len());
    for mut selected in selected {
        let project = projects
            .get(&selected.inbox.message.project_id)
            .ok_or_else(|| {
                (
                    500,
                    "Unified inbox returned an unselected project".to_string(),
                )
            })?;
        let reply_roots = roots
            .get(&selected.inbox.message.project_id)
            .unwrap_or(&empty_roots);
        let mut aggregate = None;
        for receipt in selected.recipients {
            selected.inbox.kind = receipt.kind;
            selected.inbox.read_ts = receipt.read;
            selected.inbox.ack_ts = receipt.ack;
            if let Some(aggregate) = &mut aggregate {
                UnifiedMessageAggregate::absorb(aggregate, &receipt.name, &selected.inbox);
            } else {
                aggregate = Some(UnifiedMessageAggregate::from_inbox_row(
                    project,
                    &receipt.name,
                    &selected.inbox,
                    reply_roots,
                ));
            }
        }
        out.push(aggregate.ok_or_else(|| {
            (
                500,
                "Unified inbox message has no visible recipient".to_string(),
            )
        })?);
    }
    Ok(out)
}

async fn fetch(
    cx: &Cx,
    pool: &DbPool,
    roster: &Roster,
    limit: usize,
    importance: Option<&str>,
) -> Outcome<Vec<SelectedMessage>, DbError> {
    if cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    if i64::try_from(limit).is_err() {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    }
    if limit == 0 || roster.values().all(BTreeSet::is_empty) {
        return Outcome::Ok(Vec::new());
    }
    let connection = match pool.acquire(cx).await {
        Outcome::Ok(connection) => connection,
        Outcome::Err(error) => return Outcome::Err(database_error(error)),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let mut checkout = ReadCheckout::new(connection, |connection| {
        if let Err(error) = connection.detach().close_without_checkpoint_sync() {
            tracing::warn!(%error, "unified inbox failed checkout retirement");
        }
    });
    let checked = CheckedRead {
        connection: &**checkout
            .connection
            .as_ref()
            .expect("live unified inbox checkout"),
        cx,
        interrupted: std::cell::Cell::new(false),
    };
    let result = in_snapshot(&checked, || read(&checked, roster, limit, importance));
    if checked.interrupted.get() || cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    match result {
        Ok(rows) => {
            checkout.retire = None;
            Outcome::Ok(rows)
        }
        Err(error) => Outcome::Err(error),
    }
}

fn lookup(
    conn: &impl SyncQuery,
    table: &str,
    key: &str,
    columns: &str,
    ids: &[i64],
) -> Result<Vec<Row>, DbError> {
    if ids.is_empty() || ids.len() > PAGE_SIZE {
        return Err(DbError::Internal(
            "unified inbox lookup exceeded its key bound".into(),
        ));
    }
    let binds = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let params: Vec<_> = ids.iter().copied().map(Value::BigInt).collect();
    conn.query_sync(
        &format!("SELECT {columns} FROM {table} WHERE {key} IN ({binds})"),
        &params,
    )
    .map_err(database_error)
}

fn integer(row: &Row, column: &str) -> Result<i64, DbError> {
    row.get_named(column).map_err(database_error)
}

fn current_viewers(
    conn: &impl SyncQuery,
    roster: &Roster,
) -> Result<BTreeMap<i64, BTreeMap<i64, Viewer>>, DbError> {
    let ids: Vec<_> = roster
        .values()
        .flat_map(|ids| ids.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut viewers: BTreeMap<i64, BTreeMap<i64, Viewer>> = BTreeMap::new();
    for chunk in ids.chunks(PAGE_SIZE) {
        let mut seen = BTreeSet::new();
        for row in lookup(
            conn,
            "agents",
            "id",
            "id, project_id, name, inception_ts, retired_at, contact_policy",
            chunk,
        )? {
            let id = integer(&row, "id")?;
            if chunk.binary_search(&id).is_err() || !seen.insert(id) {
                return Err(DbError::Internal(
                    "unrequested or duplicate unified inbox viewer".into(),
                ));
            }
            let project = integer(&row, "project_id")?;
            if !roster.get(&project).is_some_and(|ids| ids.contains(&id)) {
                continue;
            }
            let policy = row
                .get_named::<Option<String>>("contact_policy")
                .map_err(database_error)?;
            let viewer = Viewer {
                name: row.get_named("name").map_err(database_error)?,
                inception: integer(&row, "inception_ts")?,
                retired: row
                    .get_named::<Option<i64>>("retired_at")
                    .map_err(database_error)?
                    .is_some(),
                blocked: policy
                    .as_deref()
                    .is_some_and(|policy| policy.eq_ignore_ascii_case("block_all")),
            };
            viewers.entry(project).or_default().insert(id, viewer);
        }
    }
    Ok(viewers)
}

fn read(
    conn: &impl SyncQuery,
    roster: &Roster,
    limit: usize,
    importance: Option<&str>,
) -> Result<Vec<SelectedMessage>, DbError> {
    if limit == 0 || roster.values().all(BTreeSet::is_empty) {
        return Ok(Vec::new());
    }
    i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    let viewers = current_viewers(conn, roster)?;
    if viewers.is_empty() {
        return Ok(Vec::new());
    }
    let shared = ExplorerQuery::shared_mailbox_schema_available(conn)?;
    let mut window: BTreeMap<(i64, i64), Candidate> = BTreeMap::new();
    let mut ceiling = None;
    let mut skip_at_ceiling = 0_i64;
    let mut last_key = None;
    loop {
        let (sql, params) = ceiling.map_or_else(
            || (FIRST_HEADERS, Vec::new()),
            |created| {
                (
                    NEXT_HEADERS,
                    vec![Value::BigInt(created), Value::BigInt(skip_at_ceiling)],
                )
            },
        );
        let rows = conn.query_sync(sql, &params).map_err(database_error)?;
        if rows.len() > PAGE_SIZE {
            return Err(DbError::Internal(
                "unified inbox header page exceeded its bound".into(),
            ));
        }
        let finished = rows.len() < PAGE_SIZE;
        let mut candidates = BTreeMap::new();
        let mut last_ts = ceiling;
        let mut trailing_count = skip_at_ceiling;
        for row in rows {
            let id = integer(&row, "id")?;
            let created = integer(&row, "created_ts")?;
            let key = (created, id);
            if last_key.is_some_and(|previous| key >= previous) {
                return Err(DbError::Internal(
                    "unified inbox header cursor did not advance".into(),
                ));
            }
            last_key = Some(key);
            // Count every header before visibility filtering. The next range
            // starts at this timestamp and skips only its already visited ties.
            if last_ts == Some(created) {
                trailing_count = trailing_count.checked_add(1).ok_or_else(|| {
                    DbError::Internal("unified inbox timestamp cursor overflow".into())
                })?;
            } else {
                last_ts = Some(created);
                trailing_count = 1;
            }
            let project = integer(&row, "project_id")?;
            if !viewers.contains_key(&project) {
                continue;
            }
            let importance_value: String = row.get_named("importance").map_err(database_error)?;
            if !matches_importance_filter(&importance_value, importance) {
                continue;
            }
            if window.len() == limit
                && window
                    .first_key_value()
                    .is_some_and(|(oldest, _)| (created, id) <= *oldest)
            {
                continue;
            }
            candidates.insert(
                id,
                Candidate {
                    header: Header {
                        id,
                        project,
                        sender: integer(&row, "sender_id")?,
                        created,
                        importance: importance_value,
                    },
                    recipients: BTreeMap::new(),
                },
            );
        }
        if !candidates.is_empty() {
            recipients(conn, &viewers, &mut candidates, shared)?;
            for (_, candidate) in candidates {
                if candidate.recipients.is_empty() {
                    continue;
                }
                window.insert((candidate.header.created, candidate.header.id), candidate);
                if window.len() > limit {
                    window.pop_first();
                }
            }
        }
        // Later pages contain strictly smaller (timestamp, id) keys, including
        // within a timestamp cohort. A full visible window is already final.
        if finished || window.len() == limit {
            break;
        }
        ceiling = last_ts;
        skip_at_ceiling = trailing_count;
    }
    hydrate(conn, window.into_values().rev().collect())
}

fn recipients(
    conn: &impl SyncQuery,
    viewers: &BTreeMap<i64, BTreeMap<i64, Viewer>>,
    candidates: &mut BTreeMap<i64, Candidate>,
    shared: bool,
) -> Result<(), DbError> {
    let ids: Vec<_> = candidates.keys().copied().collect();
    for row in lookup(
        conn,
        "message_recipients",
        "message_id",
        "message_id, agent_id, kind, read_ts, ack_ts",
        &ids,
    )? {
        let message = integer(&row, "message_id")?;
        let candidate = candidates
            .get_mut(&message)
            .ok_or_else(|| DbError::Internal("unrequested direct unified inbox delivery".into()))?;
        let agent = integer(&row, "agent_id")?;
        let Some(viewer) = viewers
            .get(&candidate.header.project)
            .and_then(|project| project.get(&agent))
        else {
            continue;
        };
        let receipt = Receipt {
            name: viewer.name.clone(),
            kind: row.get_named("kind").map_err(database_error)?,
            shared: false,
            read: row.get_named("read_ts").map_err(database_error)?,
            ack: row.get_named("ack_ts").map_err(database_error)?,
        };
        if candidate.recipients.insert(agent, receipt).is_some() {
            return Err(DbError::Internal(
                "duplicate direct unified inbox receipt".into(),
            ));
        }
    }
    if !shared {
        return Ok(());
    }
    let mut shared_messages = BTreeSet::new();
    for row in lookup(
        conn,
        "project_mailbox_deliveries",
        "message_id",
        "message_id, project_id",
        &ids,
    )? {
        let id = integer(&row, "message_id")?;
        let candidate = candidates
            .get_mut(&id)
            .ok_or_else(|| DbError::Internal("unrequested shared unified inbox delivery".into()))?;
        if integer(&row, "project_id")? != candidate.header.project {
            continue;
        }
        if !shared_messages.insert(id) {
            return Err(DbError::Internal(
                "duplicate shared unified inbox delivery".into(),
            ));
        }
        for (&agent, viewer) in &viewers[&candidate.header.project] {
            if agent == candidate.header.sender
                || viewer.retired
                || viewer.blocked
                || viewer.inception > candidate.header.created
            {
                continue;
            }
            candidate
                .recipients
                .entry(agent)
                .or_insert_with(|| Receipt {
                    name: viewer.name.clone(),
                    kind: "project".into(),
                    shared: true,
                    read: None,
                    ack: None,
                });
        }
    }
    if shared_messages.is_empty() {
        return Ok(());
    }
    let shared_ids: Vec<_> = shared_messages.into_iter().collect();
    let mut seen = BTreeSet::new();
    for row in lookup(
        conn,
        "project_mailbox_receipts",
        "message_id",
        "message_id, agent_id, read_ts, ack_ts",
        &shared_ids,
    )? {
        let id = integer(&row, "message_id")?;
        if shared_ids.binary_search(&id).is_err() {
            return Err(DbError::Internal(
                "unrequested shared unified inbox receipt".into(),
            ));
        }
        let agent = integer(&row, "agent_id")?;
        let Some(receipt) = candidates
            .get_mut(&id)
            .and_then(|candidate| candidate.recipients.get_mut(&agent))
        else {
            continue;
        };
        if !receipt.shared {
            continue;
        }
        if !seen.insert((id, agent)) {
            return Err(DbError::Internal(
                "duplicate shared unified inbox receipt".into(),
            ));
        }
        receipt.read = row.get_named("read_ts").map_err(database_error)?;
        receipt.ack = row.get_named("ack_ts").map_err(database_error)?;
    }
    Ok(())
}

fn hydrate(
    conn: &impl SyncQuery,
    candidates: Vec<Candidate>,
) -> Result<Vec<SelectedMessage>, DbError> {
    let ids: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.header.id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut messages = HashMap::new();
    for chunk in ids.chunks(PAGE_SIZE) {
        for row in lookup(
            conn,
            "messages",
            "id",
            "id, project_id, sender_id, thread_id, topic, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments",
            chunk,
        )? {
            let id = integer(&row, "id")?;
            let message = MessageRow {
                id: Some(id),
                project_id: integer(&row, "project_id")?,
                sender_id: integer(&row, "sender_id")?,
                thread_id: row.get_named("thread_id").map_err(database_error)?,
                topic: row.get_named("topic").map_err(database_error)?,
                subject: row.get_named("subject").map_err(database_error)?,
                body_md: row.get_named("body_md").map_err(database_error)?,
                importance: row.get_named("importance").map_err(database_error)?,
                ack_required: integer(&row, "ack_required")?,
                created_ts: integer(&row, "created_ts")?,
                recipients_json: row.get_named("recipients_json").map_err(database_error)?,
                attachments: row.get_named("attachments").map_err(database_error)?,
            };
            if chunk.binary_search(&id).is_err() || messages.insert(id, message).is_some() {
                return Err(DbError::Internal(
                    "unrequested or duplicate unified inbox body".into(),
                ));
            }
        }
    }
    let sender_ids: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.header.sender)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut senders = HashMap::new();
    for chunk in sender_ids.chunks(PAGE_SIZE) {
        for row in lookup(conn, "agents", "id", "id, name", chunk)? {
            let id = integer(&row, "id")?;
            let name = row
                .get_named::<Option<String>>("name")
                .map_err(database_error)?
                .unwrap_or_else(|| UNKNOWN_SENDER_DISPLAY.to_string());
            if chunk.binary_search(&id).is_err() || senders.insert(id, name).is_some() {
                return Err(DbError::Internal(
                    "unrequested or duplicate unified inbox sender".into(),
                ));
            }
        }
    }
    candidates
        .into_iter()
        .map(|candidate| {
            let header = candidate.header;
            let message = messages.remove(&header.id).ok_or_else(|| {
                DbError::Internal("unified inbox body disappeared inside its snapshot".into())
            })?;
            if message.project_id != header.project
                || message.sender_id != header.sender
                || message.created_ts != header.created
                || message.importance != header.importance
            {
                return Err(DbError::Internal(
                    "unified inbox message changed inside its snapshot".into(),
                ));
            }
            Ok(SelectedMessage {
                inbox: InboxRow {
                    message,
                    kind: String::new(),
                    sender_name: senders
                        .get(&header.sender)
                        .cloned()
                        .unwrap_or_else(|| UNKNOWN_SENDER_DISPLAY.to_string()),
                    read_ts: None,
                    ack_ts: None,
                },
                recipients: candidate.recipients.into_values().collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::{DbConn, DbConnGuard};
    use std::cell::{Cell, RefCell};

    fn fixture() -> (tempfile::TempDir, DbConnGuard) {
        let dir = tempfile::tempdir().unwrap();
        let conn = mcp_agent_mail_db::guard_db_conn(
            DbConn::open_file(dir.path().join("unified.sqlite3").to_str().unwrap()).unwrap(),
            "message-first unified inbox fixture",
        );
        for sql in [
            "PRAGMA journal_mode=WAL",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, inception_ts INTEGER, retired_at INTEGER, contact_policy TEXT)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, created_ts INTEGER, importance TEXT, thread_id TEXT, topic TEXT, subject TEXT DEFAULT 'subject', body_md TEXT DEFAULT 'body', ack_required INTEGER DEFAULT 1, recipients_json TEXT DEFAULT '{}', attachments TEXT DEFAULT '[]')",
            "CREATE INDEX idx_messages_created_ts ON messages(created_ts)",
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "CREATE INDEX recipients_by_message ON message_recipients(message_id)",
            "CREATE TABLE project_mailbox_deliveries (message_id INTEGER PRIMARY KEY, project_id INTEGER)",
            "CREATE TABLE project_mailbox_receipts (message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "INSERT INTO agents VALUES (10,1,'Sender',0,NULL,'auto'), (11,1,'BlueLake',0,NULL,'auto'), (12,1,'GoldFox',0,NULL,'auto'), (13,1,'LateFox',10000,NULL,'auto'), (14,1,'BlockedFox',0,NULL,'BLOCK_ALL'), (15,1,'RetiredFox',0,1,'auto'), (21,2,'BlueLake',0,NULL,'auto')",
            "INSERT INTO messages (id,project_id,sender_id,created_ts,importance) VALUES (1,1,10,100,'urgent'), (2,1,10,400,'normal'), (3,1,10,300,'high'), (4,2,10,500,'urgent'), (5,1,10,600,'normal')",
            "INSERT INTO message_recipients VALUES (1,12,'bcc',NULL,NULL), (2,11,'to',7,8), (2,12,'cc',NULL,NULL), (3,11,'bcc',NULL,NULL), (4,21,'to',9,10)",
            "INSERT INTO project_mailbox_deliveries VALUES (1,1), (3,1), (5,2)",
            "INSERT INTO project_mailbox_receipts VALUES (1,11,11,12), (3,11,99,99), (1,21,88,88)",
        ] {
            conn.execute_raw(sql).unwrap();
        }
        (dir, conn)
    }

    fn roster() -> Roster {
        BTreeMap::from([
            (1, BTreeSet::from([10, 11, 12, 13, 14, 15])),
            (2, BTreeSet::from([21])),
        ])
    }

    fn ids(rows: &[SelectedMessage]) -> Vec<i64> {
        rows.iter()
            .map(|row| row.inbox.message.id.unwrap())
            .collect()
    }

    fn page(
        conn: &impl SyncQuery,
        roster: &Roster,
        limit: usize,
        importance: Option<&str>,
    ) -> Vec<SelectedMessage> {
        in_snapshot(conn, || read(conn, roster, limit, importance)).unwrap()
    }

    fn receipt<'a>(rows: &'a [SelectedMessage], id: i64, name: &str) -> &'a Receipt {
        rows.iter()
            .find(|row| row.inbox.message.id == Some(id))
            .unwrap()
            .recipients
            .iter()
            .find(|receipt| receipt.name == name)
            .unwrap()
    }

    #[test]
    fn message_first_window_preserves_all_recipients_and_direct_precedence() {
        let (_dir, conn) = fixture();
        let rows = page(&*conn, &roster(), 10, None);
        assert_eq!(ids(&rows), [4, 2, 3, 1]);
        assert_eq!(
            rows[1].recipients.len(),
            2,
            "one message must not consume two page slots"
        );
        assert_eq!(receipt(&rows, 2, "BlueLake").read, Some(7));
        assert_eq!(receipt(&rows, 2, "GoldFox").read, None);
        assert_eq!(receipt(&rows, 3, "BlueLake").kind, "bcc");
        assert_eq!(
            receipt(&rows, 3, "BlueLake").read,
            None,
            "direct receipt wins over shared read/ack"
        );
        assert_eq!(receipt(&rows, 3, "BlueLake").ack, None);
        assert_eq!(receipt(&rows, 1, "BlueLake").read, Some(11));
        assert_eq!(
            receipt(&rows, 1, "GoldFox").kind,
            "bcc",
            "operator view preserves explicit BCC recipient"
        );
        for id in [1, 3] {
            let row = rows
                .iter()
                .find(|row| row.inbox.message.id == Some(id))
                .unwrap();
            assert_eq!(
                row.recipients.len(),
                2,
                "exclude sender, late, retired, blocked and foreign viewers"
            );
        }
        assert_eq!(receipt(&rows, 4, "BlueLake").read, Some(9));
    }

    #[test]
    fn supplied_roster_never_expands_and_rebound_ids_do_not_cross_projects() {
        let (_dir, conn) = fixture();
        let scope = BTreeMap::from([(1, BTreeSet::from([11]))]);
        assert_eq!(ids(&page(&*conn, &scope, 10, None)), [2, 3, 1]);
        conn.execute_raw("UPDATE agents SET project_id=2 WHERE id=11")
            .unwrap();
        assert!(page(&*conn, &scope, 10, None).is_empty());
        let scope = BTreeMap::from([(1, BTreeSet::from([15]))]);
        conn.execute_raw("INSERT INTO message_recipients VALUES(2,15,'to',1,2)")
            .unwrap();
        assert_eq!(
            ids(&page(&*conn, &scope, 10, None)),
            [2],
            "explicit direct delivery to a selected retired viewer remains visible"
        );
    }

    #[test]
    fn order_uses_timestamps_and_id_ties_not_scan_or_delivery_order() {
        let (_dir, conn) = fixture();
        conn.execute_raw("UPDATE messages SET created_ts=100 WHERE id IN (1,2,3,4)")
            .unwrap();
        for limit in 1..=5 {
            let rows = page(&*conn, &roster(), limit, None);
            assert_eq!(ids(&rows), [4, 3, 2, 1][..limit.min(4)]);
        }
        conn.execute_raw("UPDATE messages SET created_ts=9000 WHERE id=1")
            .unwrap();
        assert_eq!(ids(&page(&*conn, &roster(), 2, None)), [1, 4]);
        assert_eq!(ids(&page(&*conn, &roster(), 2, Some("URGENT"))), [1, 4]);
        assert_eq!(ids(&page(&*conn, &roster(), 2, Some("high"))), [3]);
    }

    struct Traced<'a> {
        conn: &'a DbConn,
        statements: RefCell<Vec<(String, Vec<Value>)>>,
    }

    impl SyncQuery for Traced<'_> {
        fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
            self.statements
                .borrow_mut()
                .push((sql.to_string(), params.to_vec()));
            self.conn.query_sync(sql, params)
        }
        fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
            self.conn.execute_raw(sql)
        }
    }

    #[test]
    fn newest_header_pages_seek_the_timestamp_index_and_stop_at_the_visible_limit() {
        let (_dir, conn) = fixture();
        for (sql, params) in [
            (FIRST_HEADERS, Vec::new()),
            (
                NEXT_HEADERS,
                vec![Value::BigInt(i64::MAX), Value::BigInt(0)],
            ),
        ] {
            let plan: Vec<_> = conn
                .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), &params)
                .unwrap()
                .iter()
                .map(|row| row.get_as::<String>(3).unwrap())
                .collect();
            assert!(
                plan.iter()
                    .any(|detail| detail.contains("idx_messages_created_ts")),
                "header page must use the timestamp index: {plan:?}"
            );
            let opcodes: Vec<_> = conn
                .query_sync(&format!("EXPLAIN {sql}"), &params)
                .unwrap()
                .iter()
                .map(|row| row.get_as::<String>(1).unwrap())
                .collect();
            assert!(
                opcodes.iter().any(|opcode| opcode == "SeekLE"),
                "header page must seek its range boundary: {opcodes:?}"
            );
            assert!(
                opcodes
                    .iter()
                    .all(|opcode| opcode != "SorterOpen" && opcode != "Sort"),
                "header page must not sort the entire message history: {opcodes:?}"
            );
        }
        conn.execute_raw("BEGIN").unwrap();
        for id in 100..701 {
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES({id},1,10,{id}+100000,'normal'); INSERT INTO message_recipients VALUES({id},11,'to',NULL,NULL)")).unwrap();
        }
        conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES(9001,1,10,{},'normal'); INSERT INTO message_recipients VALUES(9001,11,'to',NULL,NULL)", i64::MAX)).unwrap();
        conn.execute_raw("COMMIT").unwrap();
        let traced = Traced {
            conn: &conn,
            statements: RefCell::new(Vec::new()),
        };
        let scope = BTreeMap::from([(1, BTreeSet::from([11]))]);
        assert_eq!(
            ids(&page(&traced, &scope, 7, None)),
            [9001, 700, 699, 698, 697, 696, 695]
        );
        assert_eq!(
            traced
                .statements
                .borrow()
                .iter()
                .filter(|(sql, _)| sql == FIRST_HEADERS || sql == NEXT_HEADERS)
                .count(),
            1
        );
    }

    #[test]
    fn timestamp_ties_cross_pages_without_reordering_or_scanning_the_whole_cohort() {
        let (_dir, conn) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 100..702 {
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES({id},1,10,9000,'normal'); INSERT INTO message_recipients VALUES({id},11,'to',NULL,NULL)")).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let scope = BTreeMap::from([(1, BTreeSet::from([11]))]);
        for limit in [7, 520] {
            let traced = Traced {
                conn: &conn,
                statements: RefCell::new(Vec::new()),
            };
            let expected: Vec<i64> = (100..702).rev().take(limit).collect();
            assert_eq!(ids(&page(&traced, &scope, limit, None)), expected);
            let statements = traced.statements.borrow();
            let headers: Vec<_> = statements
                .iter()
                .filter(|(sql, _)| sql == FIRST_HEADERS || sql == NEXT_HEADERS)
                .collect();
            assert_eq!(headers.len(), if limit == 7 { 1 } else { 3 });
            if limit == 520 {
                assert!(matches!(
                    headers[1].1.as_slice(),
                    [Value::BigInt(9000), Value::BigInt(256)]
                ));
                assert!(matches!(
                    headers[2].1.as_slice(),
                    [Value::BigInt(9000), Value::BigInt(512)]
                ));
            }
        }
    }

    #[test]
    fn invisible_header_pages_do_not_hide_later_visible_mail() {
        let (_dir, conn) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 100..868 {
            let project = if id >= 612 { 2 } else { 1 };
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES({id},{project},10,{id}+100000,'urgent')")).unwrap();
            let recipient = if id >= 612 {
                Some(21)
            } else if id < 356 {
                Some(12)
            } else {
                None
            };
            if let Some(agent) = recipient {
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients VALUES({id},{agent},'to',NULL,NULL)"
                ))
                .unwrap();
            }
        }
        conn.execute_raw("COMMIT").unwrap();
        let traced = Traced {
            conn: &conn,
            statements: RefCell::new(Vec::new()),
        };
        let scope = BTreeMap::from([(1, BTreeSet::from([11]))]);
        assert_eq!(ids(&page(&traced, &scope, 1, Some("urgent"))), [1]);
        let statements = traced.statements.borrow();
        assert_eq!(
            statements
                .iter()
                .filter(|(sql, _)| sql == FIRST_HEADERS || sql == NEXT_HEADERS)
                .count(),
            4,
            "foreign projects, undelivered mail and unselected recipients must all advance the cursor"
        );
        let body_reads: Vec<_> = statements
            .iter()
            .filter(|(sql, _)| sql.contains("body_md"))
            .collect();
        assert_eq!(body_reads.len(), 1);
        assert!(matches!(body_reads[0].1.as_slice(), [Value::BigInt(1)]));
    }

    #[test]
    fn important_mail_behind_ten_thousand_normal_deliveries_is_not_lost() {
        let (_dir, conn) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 100..10_101 {
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES({id},1,10,{id}+100000,'normal'); INSERT INTO message_recipients VALUES({id},11,'to',NULL,NULL)")).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let traced = Traced {
            conn: &conn,
            statements: RefCell::new(Vec::new()),
        };
        let scope = BTreeMap::from([(1, BTreeSet::from([11]))]);
        let rows = page(&traced, &scope, 1, Some("urgent"));
        assert_eq!(ids(&rows), [1]);
        assert_eq!(rows[0].inbox.message.body_md, "body");
        let statements = traced.statements.borrow();
        assert!(
            statements
                .iter()
                .filter(|(sql, _)| sql == FIRST_HEADERS || sql == NEXT_HEADERS)
                .count()
                > 39
        );
        let body_reads: Vec<_> = statements
            .iter()
            .filter(|(sql, _)| sql.contains("body_md"))
            .collect();
        assert_eq!(
            body_reads.len(),
            1,
            "discarded normal history must not hydrate any body"
        );
        assert!(matches!(body_reads[0].1.as_slice(), [Value::BigInt(1)]));
        assert!(
            statements
                .iter()
                .all(|(_, params)| params.len() <= PAGE_SIZE)
        );
    }

    #[test]
    fn malformed_discarded_body_cannot_break_a_healthy_selected_page() {
        let (_dir, conn) = fixture();
        conn.execute_raw("UPDATE messages SET body_md=X'ff' WHERE id=1")
            .unwrap();
        assert_eq!(ids(&page(&*conn, &roster(), 1, None)), [4]);
        assert!(in_snapshot(&*conn, || read(&*conn, &roster(), 10, None)).is_err());
    }

    #[test]
    fn body_hydration_batches_and_recipients_do_not_truncate_the_message_page() {
        let (_dir, conn) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 100..701 {
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES({id},1,10,{id},'normal'); INSERT INTO message_recipients VALUES({id},11,'to',NULL,NULL),({id},12,'cc',1,2)")).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let traced = Traced {
            conn: &conn,
            statements: RefCell::new(Vec::new()),
        };
        let rows = page(&traced, &roster(), 520, Some("normal"));
        assert_eq!(rows.len(), 520);
        assert_eq!(ids(&rows)[..3], [700, 699, 698]);
        assert!(rows.iter().all(|row| row.recipients.len() == 2));
        let statements = traced.statements.borrow();
        let body_reads: Vec<_> = statements
            .iter()
            .filter(|(sql, _)| sql.contains("body_md"))
            .collect();
        assert_eq!(body_reads.len(), 3);
        assert_eq!(
            body_reads
                .iter()
                .map(|(_, params)| params.len())
                .sum::<usize>(),
            520
        );
        assert!(
            statements
                .iter()
                .all(|(_, params)| params.len() <= PAGE_SIZE)
        );
        assert!(
            statements.iter().all(|(sql, _)| !sql.contains("JOIN")),
            "projection must use selected keys, not broad joins"
        );
    }

    #[test]
    fn read_only_access_does_not_create_receipts_and_partial_schema_is_not_empty_mail() {
        let (_dir, conn) = fixture();
        conn.execute_raw("PRAGMA query_only=ON").unwrap();
        assert_eq!(ids(&page(&*conn, &roster(), 10, None)), [4, 2, 3, 1]);
        for (table, expected) in [("message_recipients", 5), ("project_mailbox_receipts", 3)] {
            let rows = conn
                .query_sync(&format!("SELECT COUNT(*) AS n FROM {table}"), &[])
                .unwrap();
            assert_eq!(integer(&rows[0], "n").unwrap(), expected);
        }
        conn.execute_raw("PRAGMA query_only=OFF; ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts").unwrap();
        let failure = in_snapshot(&*conn, || read(&*conn, &roster(), 10, None));
        assert!(
            matches!(failure, Err(DbError::Sqlite(ref error)) if error.contains("incomplete project mailbox schema"))
        );
        conn.execute_raw("ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries")
            .unwrap();
        assert_eq!(ids(&page(&*conn, &roster(), 10, None)), [4, 2, 3, 1]);
        assert_eq!(
            page(&*conn, &roster(), 10, None)
                .last()
                .unwrap()
                .recipients
                .len(),
            1
        );
    }

    #[test]
    fn nested_failure_and_unwind_do_not_commit_the_callers_transaction() {
        let (_dir, conn) = fixture();
        for panic in [false, true] {
            conn.execute_raw("SAVEPOINT caller; INSERT INTO messages(id,project_id,sender_id,created_ts,importance) VALUES(999,1,10,999,'normal')").unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                in_snapshot(&*conn, || -> Result<(), DbError> {
                    assert!(!panic, "injected projection unwind");
                    Err(DbError::Internal("injected projection failure".into()))
                })
            }));
            assert_eq!(result.is_err(), panic);
            conn.execute_raw("ROLLBACK TO SAVEPOINT caller; RELEASE SAVEPOINT caller")
                .unwrap();
            assert!(
                conn.query_sync("SELECT id FROM messages WHERE id=999", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                page(&*conn, &roster(), 1, None)[0].inbox.message.id,
                Some(4)
            );
        }
    }

    #[test]
    fn expired_context_never_enters_the_read_snapshot() {
        let (_dir, conn) = fixture();
        let traced = Traced {
            conn: &conn,
            statements: RefCell::new(Vec::new()),
        };
        let cx = Cx::for_request_with_budget(
            asupersync::Budget::INFINITE.with_deadline(asupersync::Time::ZERO),
        );
        let checked = CheckedRead {
            connection: &traced,
            cx: &cx,
            interrupted: Cell::new(false),
        };
        assert!(in_snapshot(&checked, || read(&checked, &roster(), 10, None)).is_err());
        assert!(checked.interrupted.get());
        assert!(traced.statements.borrow().is_empty());
    }

    #[test]
    fn one_snapshot_keeps_headers_bodies_names_and_receipts_together() {
        let (dir, conn) = fixture();
        let writer = mcp_agent_mail_db::guard_db_conn(
            DbConn::open_file(dir.path().join("unified.sqlite3").to_str().unwrap()).unwrap(),
            "concurrent unified inbox writer",
        );
        struct Racing<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            changed: Cell<bool>,
        }
        impl SyncQuery for Racing<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let result = self.reader.query_sync(sql, params)?;
                if sql == FIRST_HEADERS && !self.changed.replace(true) {
                    self.writer.execute_raw("BEGIN; UPDATE messages SET body_md='changed' WHERE id=4; UPDATE agents SET name='Renamed' WHERE id=10; UPDATE message_recipients SET read_ts=999 WHERE message_id=4; COMMIT")?;
                }
                Ok(result)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.reader.execute_raw(sql)
            }
        }
        let racing = Racing {
            reader: &conn,
            writer: &writer,
            changed: Cell::new(false),
        };
        let before = page(&racing, &roster(), 1, None);
        assert!(racing.changed.get());
        assert_eq!(before[0].inbox.message.body_md, "body");
        assert_eq!(before[0].inbox.sender_name, "Sender");
        assert_eq!(receipt(&before, 4, "BlueLake").read, Some(9));
        let after = page(&*conn, &roster(), 1, None);
        assert_eq!(after[0].inbox.message.body_md, "changed");
        assert_eq!(after[0].inbox.sender_name, "Renamed");
        assert_eq!(receipt(&after, 4, "BlueLake").read, Some(999));
    }

    #[test]
    fn live_collector_matches_the_original_on_a_real_migrated_mailbox() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let _runtime = super::super::message_fixture_runtime();
            let pool = super::super::initialized_test_pool("unified-message-first-reference");
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            for sql in [
                "INSERT INTO projects(id,slug,human_key,created_at) VALUES(71,'unified-one','/unified-one',1),(72,'unified-two','/unified-two',1)",
                "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES(81,71,'AmberFox','test','test',1,1),(82,71,'BlueLake','test','test',1,1),(83,71,'GoldFox','test','test',1,1),(84,72,'RedFox','test','test',1,1),(85,72,'BlueLake','test','test',1,1)",
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,importance,ack_required,created_ts,recipients_json,attachments,thread_id) VALUES(1,71,81,'first','old body','urgent',1,100,'{}','[]',NULL),(2,71,81,'second','middle body','normal',1,200,'{}','[]',NULL),(3,72,84,'third','new body','high',1,300,'{}','[]','explicit-thread'),(4,71,81,'reply','reply body','normal',0,400,'{}','[]','1')",
                "INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) VALUES(1,83,'bcc',NULL,NULL),(2,82,'to',11,12),(2,83,'cc',NULL,NULL),(3,85,'to',13,14),(4,82,'to',NULL,NULL)",
                "INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts) VALUES(1,71,'to',100),(2,71,'cc',200)",
                "INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(1,82,15,16),(2,82,99,99)",
            ] {
                conn.execute_raw(sql).unwrap();
            }
            drop(conn);
            let projects = block_on_outcome(&cx, queries::list_projects(&cx, &pool)).unwrap();
            let project = |rows: Vec<UnifiedMessageAggregate>| {
                rows.into_iter()
                    .map(|row| {
                        (
                            row.id,
                            row.body_md,
                            row.thread_id,
                            row.sender,
                            row.recipients,
                            row.recipient_read,
                            row.all_read,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            for limit in [1, 2, 8] {
                for importance in [
                    None,
                    Some("urgent"),
                    Some("high"),
                    Some("normal"),
                    Some("missing"),
                ] {
                    let original = super::super::reference_collect_unified_message_aggregates(
                        &cx, &pool, &projects, limit, importance,
                    )
                    .unwrap();
                    let current = collect(&cx, &pool, &projects, limit, importance).unwrap();
                    assert_eq!(
                        project(current),
                        project(original),
                        "limit={limit} filter={importance:?}"
                    );
                }
            }
        });
    }
}
