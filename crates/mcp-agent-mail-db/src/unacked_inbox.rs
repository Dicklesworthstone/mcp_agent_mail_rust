//! Oldest-first pending acknowledgements across direct and project delivery.
//!
//! Selection, schema checks, and hydration share one read snapshot. Shared
//! receipts remain lazy and per viewer; an explicit recipient always wins,
//! including when that direct delivery has already been acknowledged.
//! Candidate reads are paged and join-free. Only the winning global window
//! loads message payloads; cancellation is checked between database calls.

use std::cell::Cell;
use std::collections::BTreeMap;

use asupersync::{CancelReason, Cx, Outcome};
use sqlmodel_core::{Error as SqlError, Row, Value};

use crate::pool::SyncQuery;
use crate::queries::{UnackedInboxRow, UNKNOWN_SENDER_DISPLAY};
use crate::sync::inbox;
use crate::{DbConn, DbError, DbPool};

const HYDRATE_BATCH: usize = 256;
const SHARED_PAGE_SIZE: usize = 128;
const DIRECT_PAGE_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? ORDER BY message_id ASC LIMIT 256";
const DIRECT_NEXT_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? AND message_id > ? \
    ORDER BY message_id ASC LIMIT 256";
const ACK_HEADER_COLUMNS: &str = "id, project_id, sender_id, created_ts, ack_required";

// Retain the previous relational definition only as a test oracle. Production
// must not ask the pinned engine to materialize history-sized multi-table joins.
#[cfg(test)]
const DIRECT_SELECTION: &str = "SELECT m.id AS id, m.created_ts AS created_ts, \
    r.kind AS kind, r.read_ts AS read_ts \
    FROM message_recipients r JOIN messages m ON m.id = r.message_id \
    WHERE r.agent_id = ?1 AND m.project_id = ?2 \
      AND m.ack_required = 1 AND r.ack_ts IS NULL";
#[cfg(test)]
const SHARED_SELECTION: &str = concat!(
    " UNION ALL SELECT m.id AS id, m.created_ts AS created_ts, \
    'project' AS kind, pr.read_ts AS read_ts \
    FROM project_mailbox_deliveries d JOIN messages m ON m.id = d.message_id \
    JOIN agents viewer ON viewer.id = ?1 \
    LEFT JOIN project_mailbox_receipts pr \
      ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
    WHERE d.project_id = ?2 AND m.ack_required = 1 AND pr.ack_ts IS NULL AND ",
    crate::project_mailbox::visible_to_viewer_sql!(),
);

fn db_error(error: impl std::fmt::Display) -> DbError {
    let message = error.to_string();
    if crate::error::is_lock_error(&message) {
        DbError::ResourceBusy(message)
    } else {
        DbError::Sqlite(message)
    }
}

fn cancelled<T>(cx: &Cx) -> Outcome<T, DbError> {
    Outcome::Cancelled(
        cx.cancel_reason()
            .unwrap_or_else(|| CancelReason::user("pending acknowledgement read cancelled")),
    )
}

struct ReadCheckout {
    connection: Option<sqlmodel_pool::PooledConnection<DbConn>>,
    reusable: bool,
}

impl Drop for ReadCheckout {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if self.reusable {
                drop(connection);
            } else if let Err(error) = connection.detach().close_without_checkpoint_sync() {
                tracing::warn!(%error, "pending acknowledgement checkout retirement failed");
            }
        }
    }
}

/// Read one oldest-first window of this viewer's pending acknowledgements.
///
/// Direct and eligible project deliveries share a single `(created_ts, id)`
/// order and limit. The supplied pool must already be admitted for the caller;
/// this reader neither opens another mailbox nor creates delivery receipts.
/// Cancellation and database failures return no partial result.
pub async fn fetch(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    fetch_with_options(cx, pool, project_id, agent_id, limit, ReadOptions::FULL).await
}

/// Read pending acknowledgement metadata without loading message bodies.
///
/// `before_ts` is an inclusive microsecond cutoff, applied before the global
/// oldest-first limit. All eligibility, read-state, and cancellation rules are
/// identical to [`fetch`]. The returned `body_md` fields are empty.
pub async fn fetch_metadata(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    before_ts: Option<i64>,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    fetch_with_options(
        cx,
        pool,
        project_id,
        agent_id,
        limit,
        ReadOptions { before_ts, include_bodies: false },
    )
    .await
}

#[derive(Clone, Copy)]
struct ReadOptions {
    before_ts: Option<i64>,
    include_bodies: bool,
}

impl ReadOptions {
    const FULL: Self = Self { before_ts: None, include_bodies: true };
}

async fn fetch_with_options(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    options: ReadOptions,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    if cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    if i64::try_from(limit).is_err() {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    }
    if limit == 0 {
        return Outcome::Ok(Vec::new());
    }
    let connection = match pool.acquire(cx).await {
        Outcome::Ok(connection) => connection,
        Outcome::Err(error) => return Outcome::Err(db_error(error)),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let mut checkout = ReadCheckout {
        connection: Some(connection),
        reusable: false,
    };
    let result = read_with_options(
        cx,
        &**checkout.connection.as_ref().expect("live acknowledgement checkout"),
        project_id,
        agent_id,
        limit,
        options,
    );
    // Failed cleanup, cancellation, and unwinding must not return an uncertain
    // session to the pool. The public result never contains a partial window.
    checkout.reusable = matches!(&result, Outcome::Ok(_));
    result
}

struct CancellableRead<'a, C> {
    connection: &'a C,
    cx: &'a Cx,
    interrupted: Cell<bool>,
}

impl<C: SyncQuery> CancellableRead<'_, C> {
    fn checkpoint(&self) -> Result<(), SqlError> {
        if self.cx.checkpoint().is_err() {
            self.interrupted.set(true);
            Err(SqlError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl<C: SyncQuery> SyncQuery for CancellableRead<'_, C> {
    fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
        self.checkpoint()?;
        let started = crate::tracking::query_timer();
        let result = self.connection.query_sync(sql, params);
        crate::tracking::record_query(sql, crate::tracking::elapsed_us(started));
        let rows = result?;
        self.checkpoint()?;
        Ok(rows)
    }

    fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
        // Release only our innermost scope, even after cancellation.
        if sql != "RELEASE SAVEPOINT am_inbox_read" {
            self.checkpoint()?;
        }
        self.connection.execute_raw(sql)
    }
}

#[cfg(test)]
fn selection_query(shared: bool, limit: usize, before_ts: Option<i64>) -> Result<String, DbError> {
    let limit = i64::try_from(limit)
        .map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    // Filter BOTH lanes before their shared oldest-first limit. The timestamp
    // remains bound; appending it only after UNION would filter just shared mail.
    let cutoff = if before_ts.is_some() { " AND m.created_ts <= ?3" } else { "" };
    let mut sql = format!("{DIRECT_SELECTION}{cutoff}");
    if shared {
        sql.push_str(SHARED_SELECTION);
        sql.push_str(cutoff);
    }
    // The pinned engine's compound SELECT outer LIMIT does not reliably use
    // bound parameters. Only this checked internal integer is interpolated;
    // every identity and cutoff remains an explicitly indexed binding.
    {
        use std::fmt::Write as _;
        let _ = write!(sql, " ORDER BY created_ts ASC, id ASC LIMIT {limit}");
    }
    Ok(sql)
}

#[cfg(test)]
fn read_from_conn(
    cx: &Cx,
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    limit: usize,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    read_with_options(cx, conn, project_id, agent_id, limit, ReadOptions::FULL)
}

fn read_with_options(
    cx: &Cx,
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    options: ReadOptions,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    if cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    if i64::try_from(limit).is_err() {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    }
    if limit == 0 {
        return Outcome::Ok(Vec::new());
    }
    let checked = CancellableRead {
        connection: conn,
        cx,
        interrupted: Cell::new(false),
    };
    let result = inbox::with_snapshot(&checked, || {
        // Reuse the public shared-schema proof rather than interpreting a
        // failed query as an empty mailbox. A partial upgrade or a view in
        // place of either table cannot publish a direct-only success.
        let shared = shared_deliveries_available(&checked, project_id)?;
        let mut selected = direct_window(&checked, project_id, agent_id, limit, options.before_ts)?;
        if shared {
            extend_shared_window(&checked, project_id, agent_id, limit, options.before_ts, &mut selected)?;
        }
        hydrate(&checked, project_id, selected, options)
    });
    if checked.interrupted.get() || cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    match result {
        Ok(rows) => Outcome::Ok(rows),
        Err(DbError::Sqlite(message)) if crate::error::is_lock_error(&message) => {
            Outcome::Err(DbError::ResourceBusy(message))
        }
        Err(error) => Outcome::Err(error),
    }
}

fn shared_deliveries_available(
    conn: &impl SyncQuery,
    project_id: i64,
) -> Result<bool, DbError> {
    if !crate::mail_explorer::ExplorerQuery::shared_mailbox_schema_available(conn)? {
        return Ok(false);
    }
    // Even an empty delivery table requires intact receipt authority. A
    // missing ACK column must not make settled shared mail look pending.
    conn.query_sync(
        "SELECT message_id, agent_id, read_ts, ack_ts FROM project_mailbox_receipts LIMIT 0",
        &[],
    )
    .map_err(db_error)?;
    let rows = conn
        .query_sync(
            "SELECT message_id FROM project_mailbox_deliveries WHERE project_id = ? LIMIT 1",
            &[Value::BigInt(project_id)],
        )
        .map_err(db_error)?;
    Ok(!rows.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Receipt {
    kind: String,
    read_ts: Option<i64>,
}

type OldestWindow = BTreeMap<(i64, i64), Receipt>;

fn receipt_timestamp(row: &Row, column: &str) -> Result<Option<i64>, DbError> {
    match row.get_by_name(column) {
        Some(Value::Null) => Ok(None),
        Some(Value::Int(value)) => Ok(Some(i64::from(*value))),
        Some(Value::BigInt(value)) => Ok(Some(*value)),
        _ => Err(DbError::Sqlite(format!("invalid pending acknowledgement receipt {column}"))),
    }
}

fn keep_oldest(window: &mut OldestWindow, key: (i64, i64), receipt: Receipt, limit: usize) {
    if window.len() < limit || window.last_key_value().is_some_and(|(old, _)| key < *old) {
        window.insert(key, receipt);
        if window.len() > limit {
            window.pop_last();
        }
    }
}

fn direct_window(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    before_ts: Option<i64>,
) -> Result<OldestWindow, DbError> {
    let mut after = None;
    let mut window = OldestWindow::new();
    loop {
        let (sql, params) = after.map_or_else(
            || (DIRECT_PAGE_SQL, vec![Value::BigInt(agent_id)]),
            |id| (DIRECT_NEXT_SQL, vec![Value::BigInt(agent_id), Value::BigInt(id)]),
        );
        let rows = conn.query_sync(sql, &params).map_err(db_error)?;
        if rows.len() > HYDRATE_BATCH {
            return Err(DbError::Internal("pending acknowledgement receipt page exceeded its bound".into()));
        }
        let finished = rows.len() < HYDRATE_BATCH;
        let mut pending = BTreeMap::new();
        for row in rows {
            let id = row.get_named::<i64>("message_id").map_err(db_error)?;
            if after.is_some_and(|previous| id <= previous) {
                return Err(DbError::Internal("pending acknowledgement receipt cursor did not advance".into()));
            }
            after = Some(id);
            if receipt_timestamp(&row, "ack_ts")?.is_none() {
                pending.insert(id, Receipt {
                    kind: row.get_named("kind").map_err(db_error)?,
                    read_ts: receipt_timestamp(&row, "read_ts")?,
                });
            }
        }
        if !pending.is_empty() {
            let ids: Vec<i64> = pending.keys().copied().collect();
            let (sql, params) = inbox::lookup_query("messages", ACK_HEADER_COLUMNS, &ids);
            let headers = conn.query_sync(&sql, &params).map_err(db_error)?;
            if headers.len() > ids.len() {
                return Err(DbError::Internal("pending acknowledgement header batch exceeded its bound".into()));
            }
            for row in headers {
                let id = row.get_named::<i64>("id").map_err(db_error)?;
                let receipt = pending.remove(&id).ok_or_else(|| {
                    DbError::Internal("unrequested or duplicate pending acknowledgement header".into())
                })?;
                let created = row.get_named::<i64>("created_ts").map_err(db_error)?;
                if row.get_named::<i64>("project_id").map_err(db_error)? == project_id
                    && row.get_named::<i64>("ack_required").map_err(db_error)? == 1
                    && before_ts.is_none_or(|cutoff| created <= cutoff)
                {
                    keep_oldest(&mut window, (created, id), receipt, limit);
                }
            }
            // Missing headers are orphan receipts, excluded by the original
            // inner join too. IDs still advance the complete receipt scan.
        }
        if finished {
            return Ok(window);
        }
    }
}

fn shared_page_query(
    project_id: i64,
    floor: i64,
    skip_at_floor: i64,
    before_ts: Option<i64>,
) -> (String, Vec<Value>) {
    let mut sql = format!("SELECT {ACK_HEADER_COLUMNS} FROM messages WHERE project_id = ? AND created_ts >= ?");
    let mut params = vec![Value::BigInt(project_id), Value::BigInt(floor)];
    if let Some(cutoff) = before_ts {
        sql.push_str(" AND created_ts <= ?");
        params.push(Value::BigInt(cutoff));
    }
    // Match the existing indexed inbox walker: a second ORDER BY column loses
    // its timestamp index. Complete each timestamp run before deciding a page
    // contains the oldest IDs. OFFSET skips only that run, not all prior pages.
    sql.push_str(" ORDER BY created_ts ASC LIMIT 128 OFFSET ?");
    params.push(Value::BigInt(skip_at_floor));
    (sql, params)
}

fn keyed_receipts(
    conn: &impl SyncQuery,
    table: &str,
    columns: &str,
    ids: &[i64],
    agent_id: i64,
) -> Result<BTreeMap<i64, Row>, DbError> {
    let mut out = BTreeMap::new();
    // The current inbox reader uses eight literal integer keys to retain the
    // pinned engine's composite-index plan. Names/projections are constants;
    // identities and cutoffs never interpolate user-provided text.
    for chunk in ids.chunks(8) {
        let keys = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT {columns} FROM {table} WHERE message_id IN ({keys}) AND agent_id = ? LIMIT 9");
        let rows = conn.query_sync(&sql, &[Value::BigInt(agent_id)]).map_err(db_error)?;
        if rows.len() > chunk.len() {
            return Err(DbError::Internal("pending acknowledgement receipt lookup exceeded its bound".into()));
        }
        for row in rows {
            let id = row.get_named::<i64>("message_id").map_err(db_error)?;
            if !chunk.contains(&id) || out.insert(id, row).is_some() {
                return Err(DbError::Internal("unrequested or duplicate pending acknowledgement receipt".into()));
            }
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_lines)]
fn extend_shared_window(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    limit: usize,
    before_ts: Option<i64>,
    window: &mut OldestWindow,
) -> Result<(), DbError> {
    // Validate receipt authority even for a retired viewer or an empty page.
    // Neither case is permission to hide a partially upgraded schema.
    conn.query_sync("SELECT message_id, agent_id, read_ts, ack_ts FROM project_mailbox_receipts LIMIT 0", &[])
        .map_err(db_error)?;
    let viewers = conn.query_sync(
        "SELECT project_id, inception_ts, retired_at, contact_policy FROM agents WHERE id = ?",
        &[Value::BigInt(agent_id)],
    ).map_err(db_error)?;
    if viewers.len() > 1 {
        return Err(DbError::Internal("duplicate pending acknowledgement viewer".into()));
    }
    let Some(viewer) = viewers.first() else { return Ok(()); };
    if viewer.get_named::<i64>("project_id").map_err(db_error)? != project_id
        || receipt_timestamp(viewer, "retired_at")?.is_some()
        || viewer.get_named::<Option<String>>("contact_policy").map_err(db_error)?
            .is_some_and(|policy| policy.eq_ignore_ascii_case("block_all"))
    {
        return Ok(());
    }
    let mut floor = viewer.get_named::<i64>("inception_ts").map_err(db_error)?;
    let mut skip_at_floor = 0_i64;
    loop {
        let (sql, params) = shared_page_query(project_id, floor, skip_at_floor, before_ts);
        let rows = conn.query_sync(&sql, &params).map_err(db_error)?;
        if rows.len() > SHARED_PAGE_SIZE {
            return Err(DbError::Internal("pending acknowledgement shared page exceeded its bound".into()));
        }
        let finished = rows.len() < SHARED_PAGE_SIZE;
        let mut last = floor;
        let mut trailing = skip_at_floor;
        let mut seen = std::collections::BTreeSet::new();
        let mut headers = BTreeMap::new();
        for row in rows {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let created = row.get_named::<i64>("created_ts").map_err(db_error)?;
            if created < last || before_ts.is_some_and(|cutoff| created > cutoff)
                || row.get_named::<i64>("project_id").map_err(db_error)? != project_id
                || !seen.insert(id)
            {
                return Err(DbError::Internal("invalid pending acknowledgement shared page".into()));
            }
            trailing = if created == last {
                trailing.checked_add(1).ok_or_else(|| DbError::Internal("pending acknowledgement timestamp offset overflow".into()))?
            } else { 1 };
            last = created;
            if row.get_named::<i64>("sender_id").map_err(db_error)? != agent_id
                && row.get_named::<i64>("ack_required").map_err(db_error)? == 1
                && (window.len() < limit || window.last_key_value().is_some_and(|(old, _)| (created, id) < *old))
            {
                headers.insert(id, created);
            }
        }
        if !headers.is_empty() {
            let keys = std::iter::repeat_n("?", headers.len()).collect::<Vec<_>>().join(",");
            let sql = format!("SELECT message_id, project_id FROM project_mailbox_deliveries WHERE message_id IN ({keys})");
            let params: Vec<Value> = headers.keys().copied().map(Value::BigInt).collect();
            let rows = conn.query_sync(&sql, &params).map_err(db_error)?;
            if rows.len() > headers.len() {
                return Err(DbError::Internal("pending acknowledgement delivery lookup exceeded its bound".into()));
            }
            let mut deliveries = BTreeMap::new();
            for row in rows {
                let id = row.get_named::<i64>("message_id").map_err(db_error)?;
                let created = headers.remove(&id).ok_or_else(|| DbError::Internal("unrequested or duplicate pending acknowledgement delivery".into()))?;
                if row.get_named::<i64>("project_id").map_err(db_error)? == project_id {
                    deliveries.insert(id, created);
                }
            }
            let ids: Vec<i64> = deliveries.keys().copied().collect();
            // Check existence, not pending status: an acknowledged direct row
            // must still suppress a second obligation through shared delivery.
            for id in keyed_receipts(conn, "message_recipients", "message_id", &ids, agent_id)?.keys() {
                deliveries.remove(id);
            }
            let ids: Vec<i64> = deliveries.keys().copied().collect();
            let mut receipts = keyed_receipts(conn, "project_mailbox_receipts", "message_id, read_ts, ack_ts", &ids, agent_id)?;
            for (id, created) in deliveries {
                let read_ts = if let Some(receipt) = receipts.remove(&id) {
                    if receipt_timestamp(&receipt, "ack_ts")?.is_some() { continue; }
                    receipt_timestamp(&receipt, "read_ts")?
                } else { None };
                keep_oldest(window, (created, id), Receipt {
                    kind: crate::project_mailbox::PROJECT_MAILBOX_KIND.into(), read_ts,
                }, limit);
            }
        }
        let complete = window.len() == limit
            && window.last_key_value().is_some_and(|((created, _), _)| last > *created);
        if finished || complete { return Ok(()); }
        if last == floor && trailing <= skip_at_floor {
            return Err(DbError::Internal("pending acknowledgement shared cursor did not advance".into()));
        }
        floor = last;
        skip_at_floor = trailing;
    }
}

fn hydrate(
    conn: &impl SyncQuery,
    project_id: i64,
    selected: OldestWindow,
    options: ReadOptions,
) -> Result<Vec<UnackedInboxRow>, DbError> {
    let mut state = BTreeMap::new();
    let mut previous = None;
    let mut ids = Vec::with_capacity(selected.len());
    for ((created, id), Receipt { kind, read_ts }) in selected {
        if options.before_ts.is_some_and(|cutoff| created > cutoff) {
            return Err(DbError::Internal("pending acknowledgement lies beyond its cutoff".into()));
        }
        if previous.is_some_and(|old| (created, id) <= old)
            || state.insert(id, (created, kind, read_ts)).is_some()
        {
            return Err(DbError::Internal("pending acknowledgements are duplicated or unordered".into()));
        }
        previous = Some((created, id));
        ids.push(id);
    }
    let mut out = Vec::with_capacity(ids.len());
    // Hydrate ONLY the winning global window, not a newest-first window later
    // reversed or limit rows independently taken from each delivery lane.
    let body = if options.include_bodies { "body_md" } else { "'' AS body_md" };
    let columns = format!(
        "id, project_id, sender_id, thread_id, topic, subject, {body}, \
         importance, ack_required, created_ts, recipients_json, attachments"
    );
    for chunk in ids.chunks(HYDRATE_BATCH) {
        let (sql, params) = inbox::lookup_query("messages", &columns, chunk);
        for row in conn.query_sync(&sql, &params).map_err(db_error)? {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let (created, kind, read_ts) = state.remove(&id).ok_or_else(|| {
                DbError::Internal("unrequested pending acknowledgement message".into())
            })?;
            let message = crate::models::MessageRow {
                id: Some(id),
                project_id: row.get_named("project_id").map_err(db_error)?,
                sender_id: row.get_named("sender_id").map_err(db_error)?,
                thread_id: row.get_named("thread_id").map_err(db_error)?,
                topic: row.get_named("topic").map_err(db_error)?,
                subject: row.get_named("subject").map_err(db_error)?,
                body_md: row.get_named("body_md").map_err(db_error)?,
                importance: row.get_named("importance").map_err(db_error)?,
                ack_required: row.get_named("ack_required").map_err(db_error)?,
                created_ts: row.get_named("created_ts").map_err(db_error)?,
                recipients_json: row.get_named("recipients_json").map_err(db_error)?,
                attachments: row.get_named("attachments").map_err(db_error)?,
            };
            if message.project_id != project_id
                || message.created_ts != created
                || message.ack_required != 1
            {
                return Err(DbError::Internal("pending acknowledgement source changed inside its snapshot".into()));
            }
            out.push(UnackedInboxRow {
                message,
                kind,
                sender_name: UNKNOWN_SENDER_DISPLAY.into(),
                read_ts,
            });
        }
    }
    if !state.is_empty() {
        return Err(DbError::Internal("selected pending acknowledgement message disappeared".into()));
    }
    let mut senders: Vec<i64> = out.iter().map(|row| row.message.sender_id).collect();
    senders.sort_unstable();
    senders.dedup();
    let mut names = BTreeMap::new();
    for chunk in senders.chunks(HYDRATE_BATCH) {
        let (sql, params) = inbox::lookup_query("agents", "id, name", chunk);
        for row in conn.query_sync(&sql, &params).map_err(db_error)? {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            if chunk.binary_search(&id).is_err()
                || names.insert(id, row.get_named::<String>("name").map_err(db_error)?).is_some()
            {
                return Err(DbError::Internal("invalid acknowledgement sender lookup".into()));
            }
        }
    }
    for row in &mut out {
        if let Some(name) = names.get(&row.message.sender_id) {
            row.sender_name.clone_from(name);
        }
    }
    out.sort_unstable_by_key(|row| (row.message.created_ts, row.message.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;

    fn run<F: Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn fixture() -> (tempfile::TempDir, crate::DbConnGuard, DbPool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unacked.sqlite3");
        let conn = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "pending acknowledgement fixture",
        );
        conn.execute_raw(crate::schema::PRAGMA_DB_INIT_SQL).unwrap();
        run(async {
            crate::schema::migrate_to_latest_base(&Cx::for_testing(), &*conn)
                .await
                .into_result()
                .unwrap();
        });
        conn.execute_raw(
            "INSERT INTO projects(id,slug,human_key,created_at) VALUES \
             (1,'one','/one',0),(2,'two','/two',0); \
             INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
             (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',100,0), \
             (3,1,'RedFox','test','test',0,0),(4,2,'GoldLeaf','test','test',0,0); \
             INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) VALUES \
             (1,1,1,'oldest shared','body-1',100,1), \
             (2,1,1,'direct','body-2',200,1), \
             (3,1,1,'shared','body-3',200,1), \
             (4,1,1,'direct and shared','body-4',300,1), \
             (5,1,1,'settled direct and shared','body-5',400,1), \
             (6,1,1,'before registration','body-6',99,1), \
             (7,1,2,'own shared message','body-7',100,1), \
             (8,1,1,'ack optional','body-8',100,0), \
             (9,2,4,'foreign project','body-9',50,1); \
             INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) VALUES \
             (2,2,'cc',123,NULL),(4,2,'bcc',NULL,NULL),(5,2,'to',321,321), \
             (9,4,'to',NULL,NULL); \
             INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts) VALUES \
             (1,1,'to',900),(3,1,'cc',800),(4,1,'to',700),(5,1,'to',600), \
             (6,1,'to',500),(7,1,'to',400),(8,1,'to',300),(9,2,'to',200); \
             INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES \
             (3,2,456,NULL),(4,2,777,777),(1,3,888,888)",
        )
        .unwrap();
        let pool = crate::create_pool(&crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", path.display()),
            min_connections: 0,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        })
        .unwrap();
        (directory, conn, pool)
    }

    fn read(conn: &impl SyncQuery, agent_id: i64, limit: usize) -> Vec<UnackedInboxRow> {
        read_from_conn(&Cx::for_testing(), conn, 1, agent_id, limit)
            .into_result()
            .unwrap()
    }

    fn ids(rows: &[UnackedInboxRow]) -> Vec<i64> {
        rows.iter().map(|row| row.message.id.unwrap()).collect()
    }

    #[test]
    fn public_pending_reader_merges_direct_and_shared_before_global_limit() {
        let (_directory, conn, pool) = fixture();
        let before = conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap();
        run(async {
            let cx = Cx::for_testing();
            for limit in [0, 1, 2, 3, 4, 100] {
                let rows = fetch(&cx, &pool, 1, 2, limit)
                    .await
                    .into_result()
                    .unwrap();
                assert_eq!(ids(&rows), [1, 2, 3, 4][..limit.min(4)]);
                for row in rows {
                    assert_eq!(row.message.body_md, format!("body-{}", row.message.id.unwrap()));
                    assert_eq!(row.sender_name, "BlueLake");
                }
            }
        });
        let after = conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap();
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
    }

    #[test]
    fn each_viewer_keeps_their_receipts_and_direct_delivery_takes_precedence() {
        let (_directory, conn, _pool) = fixture();
        let rows = read(&*conn, 2, 100);
        assert_eq!(ids(&rows), [1, 2, 3, 4]);
        assert_eq!(rows[0].kind, "project");
        assert_eq!(rows[0].read_ts, None);
        assert_eq!(rows[1].kind, "cc");
        assert_eq!(rows[1].read_ts, Some(123));
        assert_eq!(rows[2].read_ts, Some(456));
        assert_eq!(rows[3].kind, "bcc");
        assert_eq!(rows[3].read_ts, None);
        // The ACK held by viewer 3 neither settles nor creates viewer 2's row.
        assert!(!ids(&read(&*conn, 3, 100)).contains(&1));
        // An acknowledged direct receipt must not reappear via a missing
        // shared receipt. Conversely a shared ACK cannot settle direct mail.
        assert!(!ids(&rows).contains(&5));
        assert!(ids(&rows).contains(&4));
    }

    #[test]
    fn acknowledging_shared_mail_removes_only_that_obligation() {
        let (_directory, conn, _pool) = fixture();
        for (sql, params) in crate::project_mailbox::receipt_write_statements(
            crate::project_mailbox::ReceiptUpdate::Acknowledge, 3, 2, 999,
        ) {
            conn.execute_sync(sql, &params).unwrap();
        }
        assert_eq!(ids(&read(&*conn, 2, 100)), [1, 2, 4]);
        assert!(ids(&read(&*conn, 3, 100)).contains(&3));
    }

    #[test]
    fn late_join_retirement_blocked_sender_and_foreign_project_do_not_create_shared_debt() {
        let (_directory, conn, _pool) = fixture();
        assert!(!ids(&read(&*conn, 2, 100)).iter().any(|id| [6, 7, 8, 9].contains(id)));
        assert!(read(&*conn, 1, 100).iter().all(|row| row.message.sender_id != 1));
        assert!(read(&*conn, 4, 100).is_empty());
        conn.execute_raw("UPDATE agents SET retired_at=1000 WHERE id=2").unwrap();
        assert_eq!(ids(&read(&*conn, 2, 100)), [2, 4]);
        conn.execute_raw("UPDATE agents SET retired_at=NULL, contact_policy='BLOCK_ALL' WHERE id=2").unwrap();
        assert_eq!(ids(&read(&*conn, 2, 100)), [2, 4]);
        conn.execute_raw("UPDATE agents SET contact_policy='auto', inception_ts=1000 WHERE id=2").unwrap();
        assert_eq!(ids(&read(&*conn, 2, 100)), [2, 4]);
    }

    #[test]
    fn legacy_absence_is_allowed_but_incomplete_shared_schema_is_not() {
        for missing in ["receipts", "deliveries", "both"] {
            let (_directory, conn, _pool) = fixture();
            conn.execute_raw("PRAGMA foreign_keys=OFF").unwrap();
            if missing != "deliveries" {
                conn.execute_raw("DROP TABLE project_mailbox_receipts").unwrap();
            }
            if missing != "receipts" {
                conn.execute_raw("DROP TABLE project_mailbox_deliveries").unwrap();
            }
            let result = read_from_conn(&Cx::for_testing(), &*conn, 1, 2, 100);
            if missing == "both" {
                assert_eq!(ids(&result.into_result().unwrap()), [2, 4]);
            } else {
                assert!(matches!(result, Outcome::Err(_)), "{missing}");
            }
        }
    }

    #[test]
    fn failure_releases_only_the_readers_nested_scope() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        conn.execute_raw("UPDATE messages SET body_md='uncommitted outer write' WHERE id=2").unwrap();
        assert_eq!(read(&*conn, 2, 100)[1].message.body_md, "uncommitted outer write");
        conn.execute_raw("UPDATE project_mailbox_receipts SET read_ts='bad' WHERE message_id=3 AND agent_id=2").unwrap();
        assert!(matches!(
            read_from_conn(&Cx::for_testing(), &*conn, 1, 2, 100),
            Outcome::Err(_)
        ));
        assert_eq!(
            conn.query_sync("SELECT body_md FROM messages WHERE id=2", &[]).unwrap()[0]
                .get_named::<String>("body_md").unwrap(),
            "uncommitted outer write"
        );
        conn.execute_raw("ROLLBACK").unwrap();
        assert_eq!(read(&*conn, 2, 100)[1].message.body_md, "body-2");
    }

    struct Recording<'a> {
        conn: &'a DbConn,
        calls: RefCell<Vec<(String, usize, usize)>>,
    }

    impl SyncQuery for Recording<'_> {
        fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
            let rows = self.conn.query_sync(sql, params)?;
            self.calls.borrow_mut().push((sql.into(), params.len(), rows.len()));
            Ok(rows)
        }
        fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
            self.conn.execute_raw(sql)
        }
    }

    #[test]
    fn sparse_shared_oldest_message_is_not_lost_behind_large_direct_history() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for n in 10..=320 {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) \
                 VALUES({n},1,1,'newer','body-{n}',{},1); \
                 INSERT INTO message_recipients(message_id,agent_id,kind) VALUES({n},2,'to')",
                1000 + n,
            )).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let recording = Recording { conn: &conn, calls: RefCell::new(Vec::new()) };
        let rows = read(&recording, 2, 1);
        assert_eq!(ids(&rows), [1]);
        let calls = recording.calls.borrow();
        let body_reads: Vec<_> = calls.iter().filter(|(sql, _, _)| sql.contains("body_md")).collect();
        assert_eq!(body_reads.len(), 1);
        assert_eq!(body_reads[0].1, 1);
        assert_eq!(body_reads[0].2, 1);
        drop(calls);
        let rows = read(&recording, 2, 300);
        assert_eq!(rows.len(), 300);
        assert_eq!(ids(&rows)[..4], [1, 2, 3, 4]);
        assert_eq!(rows.last().unwrap().message.id, Some(305));
        assert!(recording.calls.borrow().iter().all(|(_, binds, _)| *binds <= HYDRATE_BATCH));
    }

    #[test]
    fn malformed_receipt_and_missing_message_sources_are_not_partial_successes() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("UPDATE project_mailbox_receipts SET read_ts='not a timestamp' WHERE message_id=3 AND agent_id=2").unwrap();
        assert!(matches!(read_from_conn(&Cx::for_testing(), &*conn, 1, 2, 100), Outcome::Err(_)));
        // The reader released its scope even on decode failure.
        conn.execute_raw("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }

    #[test]
    fn zero_and_overflow_limits_are_resolved_without_database_io() {
        let (_directory, conn, _pool) = fixture();
        let recording = Recording { conn: &conn, calls: RefCell::new(Vec::new()) };
        assert!(read(&recording, 2, 0).is_empty());
        #[cfg(target_pointer_width = "64")]
        assert!(matches!(
            read_from_conn(&Cx::for_testing(), &recording, 1, 2, usize::MAX),
            Outcome::Err(DbError::InvalidArgument { field: "limit", .. })
        ));
        assert!(recording.calls.borrow().is_empty());
    }

    #[test]
    fn cancellation_after_real_selection_preserves_the_callers_transaction() {
        struct Interrupt<'a> {
            conn: &'a DbConn,
            cx: &'a Cx,
            fired: Cell<bool>,
        }
        impl SyncQuery for Interrupt<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.conn.query_sync(sql, params)?;
                if sql == DIRECT_PAGE_SQL && !self.fired.replace(true) {
                    self.cx.set_cancel_requested(true);
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.conn.execute_raw(sql)
            }
        }
        let (_directory, conn, _pool) = fixture();
        for outer_transaction in [false, true] {
            if outer_transaction {
                conn.execute_raw("BEGIN; UPDATE messages SET subject='caller write' WHERE id=2").unwrap();
            } else {
                conn.execute_raw("PRAGMA query_only=ON").unwrap();
            }
            let cx = Cx::for_testing();
            let interrupt = Interrupt { conn: &conn, cx: &cx, fired: Cell::new(false) };
            assert!(matches!(
                read_from_conn(&cx, &interrupt, 1, 2, 100),
                Outcome::Cancelled(_)
            ));
            assert!(interrupt.fired.get());
            if outer_transaction {
                assert_eq!(
                    conn.query_sync("SELECT subject FROM messages WHERE id=2", &[]).unwrap()[0]
                        .get_named::<String>("subject").unwrap(),
                    "caller write"
                );
                conn.execute_raw("ROLLBACK").unwrap();
            } else {
                assert_eq!(conn.query_sync("PRAGMA query_only", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 1);
                conn.execute_raw("PRAGMA query_only=OFF").unwrap();
            }
            conn.execute_raw("BEGIN; ROLLBACK").unwrap();
        }
    }

    #[test]
    fn peer_commit_cannot_mix_ack_selection_and_message_bodies() {
        let (directory, conn, _pool) = fixture();
        let writer = crate::guard_db_conn(
            DbConn::open_file(directory.path().join("unacked.sqlite3").to_str().unwrap()).unwrap(),
            "pending acknowledgement peer",
        );
        struct Racing<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            committed: Cell<bool>,
        }
        impl SyncQuery for Racing<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql == DIRECT_PAGE_SQL && !self.committed.replace(true) {
                    self.writer.execute_raw(
                        "BEGIN IMMEDIATE; UPDATE messages SET body_md='peer body' WHERE id=3; \
                         UPDATE project_mailbox_receipts SET ack_ts=999 WHERE message_id=3 AND agent_id=2; \
                         COMMIT",
                    )?;
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.reader.execute_raw(sql)
            }
        }
        let race = Racing { reader: &conn, writer: &writer, committed: Cell::new(false) };
        let before = read(&race, 2, 100);
        assert!(race.committed.get());
        assert_eq!(ids(&before), [1, 2, 3, 4]);
        assert_eq!(before[2].message.body_md, "body-3");
        assert_eq!(ids(&read(&*conn, 2, 100)), [1, 2, 4]);
    }

    #[test]
    fn metadata_cutoff_is_inclusive_for_both_lanes_before_the_global_limit() {
        let (_directory, conn, pool) = fixture();
        let before = conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap();
        run(async {
            let cx = Cx::for_testing();
            for (cutoff, expected) in [
                (Some(i64::MIN), vec![]),
                (Some(99), vec![]),
                (Some(100), vec![1]),
                (Some(199), vec![1]),
                (Some(200), vec![1, 2, 3]),
                (Some(300), vec![1, 2, 3, 4]),
                (Some(i64::MAX), vec![1, 2, 3, 4]),
                (None, vec![1, 2, 3, 4]),
            ] {
                for limit in [0, 1, 2, 3, 100] {
                    let rows = fetch_metadata(
                        &cx, &pool, 1, 2, limit, cutoff,
                    ).await.into_result().unwrap();
                    assert_eq!(ids(&rows), expected[..limit.min(expected.len())]);
                    assert!(rows.iter().all(|row| row.message.body_md.is_empty()));
                    assert!(rows.iter().all(|row| row.sender_name == "BlueLake"));
                }
            }
            let full = fetch(&cx, &pool, 1, 2, 100)
                .await.into_result().unwrap();
            assert!(full.iter().all(|row| row.message.body_md == format!("body-{}", row.message.id.unwrap())));
        });
        let after = conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap();
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
    }

    #[test]
    fn metadata_hydration_never_reads_the_body_column_or_changes_receipts() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_sync("UPDATE messages SET body_md = ?", &[Value::Text("b".repeat(1_000_000))]).unwrap();
        conn.execute_raw("PRAGMA query_only=ON").unwrap();
        let recording = Recording { conn: &conn, calls: RefCell::new(Vec::new()) };
        let rows = read_with_options(
            &Cx::for_testing(), &recording, 1, 2, 3,
            ReadOptions { before_ts: Some(200), include_bodies: false },
        ).into_result().unwrap();
        assert_eq!(ids(&rows), [1, 2, 3]);
        assert_eq!(rows[1].read_ts, Some(123));
        assert_eq!(rows[2].read_ts, Some(456));
        assert!(rows.iter().all(|row| row.message.body_md.is_empty()));
        let calls = recording.calls.borrow();
        let hydrations: Vec<_> = calls.iter()
            .filter(|(sql, _, _)| sql.contains("AS body_md"))
            .collect();
        assert_eq!(hydrations.len(), 1);
        assert_eq!(hydrations[0].1, 3);
        assert_eq!(hydrations[0].2, 3);
        assert!(hydrations[0].0.contains("'' AS body_md"));
        for (sql, _, _) in calls.iter() {
            assert!(!sql.replace("'' AS body_md", "").contains("body_md"));
        }
        assert_eq!(conn.query_sync("PRAGMA query_only", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 1);
    }

    #[test]
    fn metadata_cutoff_also_filters_pre_shared_schema_direct_mail() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("PRAGMA foreign_keys=OFF; DROP TABLE project_mailbox_receipts; DROP TABLE project_mailbox_deliveries").unwrap();
        for (cutoff, expected) in [(199, vec![]), (200, vec![2]), (300, vec![2, 4])] {
            let rows = read_with_options(
                &Cx::for_testing(), &*conn, 1, 2, 100,
                ReadOptions { before_ts: Some(cutoff), include_bodies: false },
            ).into_result().unwrap();
            assert_eq!(ids(&rows), expected);
            assert!(rows.iter().all(|row| row.message.body_md.is_empty()));
        }
    }

    #[test]
    fn selection_keeps_extreme_cutoffs_bound_and_the_limit_global() {
        for shared in [false, true] {
            for cutoff in [i64::MIN, -1, 0, i64::MAX] {
                let sql = selection_query(shared, 7, Some(cutoff)).unwrap();
                assert_eq!(sql.matches("m.created_ts <= ?3").count(), if shared { 2 } else { 1 });
                assert_eq!(sql.matches("LIMIT").count(), 1);
                assert!(sql.ends_with("ORDER BY created_ts ASC, id ASC LIMIT 7"));
                assert!(!sql.contains(&i64::MIN.to_string()));
                assert!(!sql.contains(&i64::MAX.to_string()));
            }
            assert!(!selection_query(shared, 7, None).unwrap().contains("?3"));
        }
    }

    #[test]
    fn cancelled_metadata_read_never_returns_an_empty_success_or_marks_mail_read() {
        let (_directory, conn, pool) = fixture();
        let before = conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        run(async {
            for limit in [0, 1, 100] {
                assert!(matches!(
                    fetch_metadata(&cx, &pool, 1, 2, limit, Some(200)).await,
                    Outcome::Cancelled(_)
                ));
            }
        });
        assert_eq!(format!("{before:?}"), format!("{:?}", conn.query_sync("SELECT * FROM project_mailbox_receipts", &[]).unwrap()));
    }

    #[test]
    fn late_direct_page_can_contain_the_oldest_message() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 1000..1700 {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) \
                 VALUES({id},1,1,'reverse chronology','body',{created},1); \
                 INSERT INTO message_recipients(message_id,agent_id,kind) VALUES({id},2,'to')",
                created = 1700 - id,
            )).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let recording = Recording { conn: &conn, calls: RefCell::new(Vec::new()) };
        assert_eq!(ids(&read(&recording, 2, 3)), [1699, 1698, 1697]);
        let calls = recording.calls.borrow();
        assert!(calls.iter().filter(|(sql, _, _)| sql == DIRECT_NEXT_SQL).count() >= 2);
        assert!(calls.iter().all(|(sql, _, rows)| !sql.contains("JOIN") && !sql.contains("UNION") && *rows <= HYDRATE_BATCH));
        assert_eq!(calls.iter().filter(|(sql, _, _)| sql.contains("body_md")).map(|(_, _, rows)| rows).sum::<usize>(), 3);
    }

    #[test]
    fn shared_timestamp_ties_cross_pages_without_assuming_id_order() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in (1000..1400).rev() {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) \
                 VALUES({id},1,1,'tied','body',100,1); \
                 INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts) \
                 VALUES({id},1,'to',{})", 2000 - id,
            )).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let recording = Recording { conn: &conn, calls: RefCell::new(Vec::new()) };
        assert_eq!(ids(&read(&recording, 2, 3)), [1, 1000, 1001]);
        let calls = recording.calls.borrow();
        assert!(calls.iter().filter(|(sql, _, _)| sql.contains("ORDER BY created_ts ASC LIMIT 128 OFFSET")).count() >= 4);
        assert!(calls.iter().all(|(sql, _, rows)| !sql.contains("JOIN") && !sql.contains("UNION") && *rows <= HYDRATE_BATCH));
    }

    #[test]
    fn bounded_reader_matches_relational_reference_across_filters_and_viewers() {
        let (_directory, conn, _pool) = fixture();
        for viewer in [1, 2, 3, 4] {
            for project in [1, 2] {
                for limit in [0, 1, 3, 30] {
                    for cutoff in [None, Some(i64::MIN), Some(100), Some(200), Some(i64::MAX)] {
                        let reference_sql = selection_query(true, limit, cutoff).unwrap();
                        let mut params = vec![Value::BigInt(viewer), Value::BigInt(project)];
                        if let Some(ts) = cutoff { params.push(Value::BigInt(ts)); }
                        let expected = conn.query_sync(&reference_sql, &params).unwrap();
                        let actual = read_with_options(&Cx::for_testing(), &*conn, project, viewer, limit,
                            ReadOptions { before_ts: cutoff, include_bodies: false }).into_result().unwrap();
                        let expected: Vec<_> = expected.iter().map(|row| (
                            row.get_named::<i64>("id").unwrap(),
                            row.get_named::<i64>("created_ts").unwrap(),
                            row.get_named::<String>("kind").unwrap(),
                            receipt_timestamp(row, "read_ts").unwrap(),
                        )).collect();
                        let actual: Vec<_> = actual.into_iter().map(|row| (
                            row.message.id.unwrap(), row.message.created_ts, row.kind, row.read_ts,
                        )).collect();
                        assert_eq!(actual, expected, "viewer={viewer} project={project} limit={limit} cutoff={cutoff:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn cancellation_between_receipt_pages_returns_no_partial_window() {
        struct CancelSecondPage<'a> { conn: &'a DbConn, cx: &'a Cx, pages: Cell<usize> }
        impl SyncQuery for CancelSecondPage<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.conn.query_sync(sql, params)?;
                if sql == DIRECT_PAGE_SQL || sql == DIRECT_NEXT_SQL {
                    self.pages.set(self.pages.get() + 1);
                    if self.pages.get() == 2 { self.cx.set_cancel_requested(true); }
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> { self.conn.execute_raw(sql) }
        }
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for id in 1000..1600 {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,ack_required) VALUES({id},1,1,'later','body',{id},1); \
                 INSERT INTO message_recipients(message_id,agent_id,kind) VALUES({id},2,'to')",
            )).unwrap();
        }
        conn.execute_raw("COMMIT; PRAGMA query_only=ON").unwrap();
        let cx = Cx::for_testing();
        let interrupted = CancelSecondPage { conn: &conn, cx: &cx, pages: Cell::new(0) };
        assert!(matches!(read_from_conn(&cx, &interrupted, 1, 2, 1), Outcome::Cancelled(_)));
        assert_eq!(interrupted.pages.get(), 2);
        assert_eq!(conn.query_sync("PRAGMA query_only", &[]).unwrap()[0].get_as::<i64>(0).unwrap(), 1);
        conn.execute_raw("PRAGMA query_only=OFF; BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }

    #[test]
    fn duplicate_receipt_page_is_refused_and_read_scope_is_released() {
        struct Duplicate<'a>(&'a DbConn);
        impl SyncQuery for Duplicate<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let mut rows = self.0.query_sync(sql, params)?;
                if sql == DIRECT_PAGE_SQL && !rows.is_empty() {
                    rows.extend(self.0.query_sync(sql, params)?);
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> { self.0.execute_raw(sql) }
        }
        let (_directory, conn, _pool) = fixture();
        let result = read_from_conn(&Cx::for_testing(), &Duplicate(&conn), 1, 2, 100);
        assert!(matches!(result, Outcome::Err(DbError::Internal(_))));
        conn.execute_raw("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }
}
