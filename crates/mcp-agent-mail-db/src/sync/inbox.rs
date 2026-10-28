//! Recipient-index-driven inbox reads (br-qctx4).
//!
//! Only one agent's receipt pages enter the read pipeline. Message headers
//! are loaded by primary key before applying filters, avoiding the engine's
//! history-sized join plans. Bodies and sender names are hydrated only for
//! the newest requested window. The caller supplies one read snapshot.

use std::collections::{BTreeMap, HashMap};

use sqlmodel_core::{Row, Value};

use super::{InboxBodyPolicy, InboxFetchOptions};
use crate::error::DbError;
use crate::models::MessageRow;
use crate::pool::SyncQuery;
use crate::queries::{InboxRow, UNKNOWN_SENDER_DISPLAY};

const PAGE_SIZE: usize = 256;
/// Receipt pages carry no ORDER BY: with one, the pinned engine seeks on
/// `agent_id` alone and walks the agent's whole receipt history for every
/// page. [`receipt_page`] sorts the rows and proves a full page is the lowest
/// 256 IDs with an index count, re-reading it ordered when it is not.
pub const RECIPIENT_PAGE_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? LIMIT 256";
pub const RECIPIENT_NEXT_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? AND message_id > ? LIMIT 256";
pub const RECIPIENT_PAGE_COUNT_SQL: &str =
    "SELECT COUNT(*) AS n FROM message_recipients WHERE agent_id = ? AND message_id <= ?";
pub const RECIPIENT_NEXT_COUNT_SQL: &str = "SELECT COUNT(*) AS n FROM message_recipients \
    WHERE agent_id = ? AND message_id > ? AND message_id <= ?";
const RECIPIENT_PAGE_ORDERED_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? ORDER BY message_id ASC LIMIT 256";
const RECIPIENT_NEXT_ORDERED_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? AND message_id > ? \
    ORDER BY message_id ASC LIMIT 256";
pub const HEADER_COLUMNS: &str =
    "id, project_id, sender_id, created_ts, importance, ack_required, topic";

fn db_error(error: impl std::fmt::Display) -> DbError {
    DbError::Sqlite(error.to_string())
}

struct ReadSnapshot<'a, C: SyncQuery> {
    conn: &'a C,
    active: bool,
}

impl<C: SyncQuery> Drop for ReadSnapshot<'_, C> {
    fn drop(&mut self) {
        if self.active {
            // Release only our nested read scope, never the caller's transaction.
            let _ = self.conn.execute_raw("RELEASE SAVEPOINT am_inbox_read");
        }
    }
}

pub fn with_snapshot<C: SyncQuery, T>(
    conn: &C,
    read: impl FnOnce() -> Result<T, DbError>,
) -> Result<T, DbError> {
    conn.execute_raw("SAVEPOINT am_inbox_read")
        .map_err(db_error)?;
    let mut snapshot = ReadSnapshot { conn, active: true };
    let result = read();
    conn.execute_raw("RELEASE SAVEPOINT am_inbox_read")
        .map_err(db_error)?;
    snapshot.active = false;
    result
}

/// Table and projection are internal constants, never user input. Keep these
/// lookups free of secondary-index predicates: a project/topic predicate can
/// make the pinned planner choose an index scan over the entire project.
#[must_use]
pub fn lookup_query(table: &str, columns: &str, ids: &[i64]) -> (String, Vec<Value>) {
    debug_assert!(!ids.is_empty() && ids.len() <= PAGE_SIZE);
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    (
        format!("SELECT {columns} FROM {table} WHERE id IN ({placeholders})"),
        ids.iter().copied().map(Value::BigInt).collect(),
    )
}

#[derive(Clone)]
struct Receipt {
    kind: String,
    read_ts: Option<i64>,
    ack_ts: Option<i64>,
}

/// The newest inbox rows kept so far, ordered by `(created_ts, id)`.
type NewestWindow = BTreeMap<(i64, i64), Receipt>;

fn query(conn: &impl SyncQuery, sql: &str, params: &[Value]) -> Result<Vec<Row>, DbError> {
    conn.query_sync(sql, params).map_err(db_error)
}

pub(super) fn fetch_direct(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_direct_with_policy(conn, project_id, agent_id, since_ts, limit, options, false)
}

pub(super) fn fetch_direct_with_policy(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
    candidates_only: bool,
) -> Result<Vec<InboxRow>, DbError> {
    i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let walked = match newest_first_budget(conn, agent_id, limit, options)? {
        Some(budget) => {
            newest_first_window(conn, project_id, agent_id, since_ts, limit, options, budget)?
        }
        None => None,
    };
    let newest = match walked {
        Some(newest) => newest,
        None => receipt_window(conn, project_id, agent_id, since_ts, limit, options)?,
    };
    hydrate(
        conn,
        project_id,
        newest,
        options.body_policy,
        candidates_only,
    )
}

/// Direct inboxes that span more than one receipt page (`PAGE_SIZE`) may try
/// the newest-first walk (br-kp1in.40).
const NEWEST_FIRST_MIN_INBOX: i64 = 256;
pub const INBOX_STATS_HINT_SQL: &str = "SELECT total_count FROM inbox_stats WHERE agent_id = ?";

/// Without a read/ACK-state filter the receipt scan reads one receipt and one
/// header per inbox row, so its cost grows with the whole inbox. Walking the
/// project's newest headers costs the same per row read but can stop once
/// the window is complete, which pays when the agent receives a good share
/// of the project's recent mail. `inbox_stats` only chooses the plan and caps
/// the walk at a quarter of the receipt count; both plans are exact, so a
/// stale count costs time, never rows. A missing count, or a mailbox without
/// the table, keeps the receipt scan. Read/ACK-state filters keep it too: it
/// skips their header lookups, while their rows can sit anywhere in history.
fn newest_first_budget(
    conn: &impl SyncQuery,
    agent_id: i64,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Option<usize>, DbError> {
    if options.unread_only || options.ack_required_only || options.ack_overdue_before.is_some() {
        return Ok(None);
    }
    let rows = match conn.query_sync(INBOX_STATS_HINT_SQL, &[Value::BigInt(agent_id)]) {
        Ok(rows) => rows,
        Err(error) if super::is_missing_inbox_stats_table_error(&error.to_string()) => {
            return Ok(None);
        }
        Err(error) => return Err(db_error(error)),
    };
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let total = row.get_named::<i64>("total_count").map_err(db_error)?;
    let wanted = i64::try_from(limit).unwrap_or(i64::MAX);
    if total < NEWEST_FIRST_MIN_INBOX || wanted.saturating_mul(4) > total {
        return Ok(None);
    }
    Ok(usize::try_from(total / 4).ok())
}

/// A page's receipts as one index range read. IDs usually follow creation
/// time, so a header page spans few of the agent's other receipts; IDs that
/// do not (imported or re-elected mail) fill this bound instead. No ORDER BY:
/// with one, the pinned engine seeks on `agent_id` alone and walks the whole
/// inbox. Every row in range is used or the page falls back, so row order and
/// which rows a full read returns do not matter.
pub const RECIPIENT_RANGE_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? AND message_id >= ? AND message_id <= ? \
    LIMIT 256";
const RECIPIENT_RANGE_LIMIT: usize = 2 * SHARED_PAGE_SIZE;

/// The agent's receipts for one page of candidate headers, and the number of
/// receipt rows read. Rows outside the candidates belong to other pages or
/// were filtered there; each page reads its own range.
fn page_receipts(
    conn: &impl SyncQuery,
    agent_id: i64,
    candidates: &HashMap<i64, i64>,
) -> Result<(Vec<(i64, Receipt)>, usize), DbError> {
    let (Some(low), Some(high)) = (
        candidates.keys().copied().min(),
        candidates.keys().copied().max(),
    ) else {
        return Ok((Vec::new(), 0));
    };
    let mut rows = query(
        conn,
        RECIPIENT_RANGE_SQL,
        &[
            Value::BigInt(agent_id),
            Value::BigInt(low),
            Value::BigInt(high),
        ],
    )?;
    // A full read may have cut off rows of this page: use key batches.
    if rows.len() >= RECIPIENT_RANGE_LIMIT {
        let ids: Vec<i64> = candidates.keys().copied().collect();
        rows = shared_receipt_rows(
            conn,
            "message_recipients",
            "message_id, kind, read_ts, ack_ts",
            &ids,
            agent_id,
        )?;
    }
    let read = rows.len();
    let mut out = Vec::new();
    for row in rows {
        let id = row.get_named::<i64>("message_id").map_err(db_error)?;
        if !candidates.contains_key(&id) {
            continue;
        }
        out.push((
            id,
            Receipt {
                kind: row.get_named("kind").map_err(db_error)?,
                read_ts: row.get_named("read_ts").map_err(db_error)?,
                ack_ts: row.get_named("ack_ts").map_err(db_error)?,
            },
        ));
    }
    Ok((out, read))
}

/// The newest window from the project's headers, newest first, or `None` once
/// `budget` rows have been read without completing it. Only header filters
/// apply here; [`newest_first_budget`] keeps read/ACK-state filters off it.
/// A page below a timestamp ceiling costs its position, not its size: the
/// pinned engine seeks the project's newest header and steps back past every
/// newer one before the first row it returns, so page `k` is charged `k` pages.
fn newest_first_window(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
    budget: usize,
) -> Result<Option<NewestWindow>, DbError> {
    if options.unread_only || options.ack_required_only || options.ack_overdue_before.is_some() {
        return Err(DbError::Internal(
            "read/ACK-state filters need the receipt scan".into(),
        ));
    }
    let mut newest = NewestWindow::new();
    let mut ceiling = None;
    let mut skip_at_ceiling = 0_i64;
    let mut work = 0_usize;
    let mut pages = 0_usize;
    loop {
        let (sql, params) = shared_page_query(project_id, ceiling, skip_at_ceiling);
        let rows = query(conn, &sql, &params)?;
        if rows.len() > SHARED_PAGE_SIZE {
            return Err(DbError::Internal(
                "direct inbox header page exceeded its bound".into(),
            ));
        }
        let finished = rows.len() < SHARED_PAGE_SIZE;
        pages += 1;
        work = work.saturating_add(pages.saturating_mul(SHARED_PAGE_SIZE));
        let mut last_ts = ceiling;
        let mut trailing_count = skip_at_ceiling;
        let mut candidates = HashMap::new();
        let mut lower_bound_reached = false;
        for row in rows {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let created = row.get_named::<i64>("created_ts").map_err(db_error)?;
            if last_ts.is_some_and(|previous| created > previous) {
                return Err(DbError::Internal(
                    "direct inbox timestamps are not descending".into(),
                ));
            }
            if last_ts == Some(created) {
                trailing_count = trailing_count.checked_add(1).ok_or_else(|| {
                    DbError::Internal("direct inbox timestamp offset overflow".into())
                })?;
            } else {
                last_ts = Some(created);
                trailing_count = 1;
            }
            if since_ts.is_some_and(|ts| created <= ts) {
                lower_bound_reached = true;
                continue;
            }
            if row.get_named::<i64>("project_id").map_err(db_error)? != project_id {
                return Err(DbError::Internal(
                    "direct inbox returned a foreign project header".into(),
                ));
            }
            let importance = row.get_named::<String>("importance").map_err(db_error)?;
            let topic = row.get_named::<Option<String>>("topic").map_err(db_error)?;
            if (options.urgent_only && !matches!(importance.as_str(), "high" | "urgent"))
                || options.topic.is_some_and(|wanted| {
                    !topic
                        .as_deref()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
                })
            {
                continue;
            }
            if candidates.insert(id, created).is_some() {
                return Err(DbError::Internal("duplicate direct inbox candidate".into()));
            }
        }
        let (receipts, read) = page_receipts(conn, agent_id, &candidates)?;
        work = work.saturating_add(read);
        for (id, receipt) in receipts {
            let key = (candidates[&id], id);
            if newest.len() < limit || newest.first_key_value().is_some_and(|(old, _)| key > *old) {
                newest.insert(key, receipt);
                if newest.len() > limit {
                    newest.pop_first();
                }
            }
        }
        // Finish the boundary timestamp even after collecting `limit` rows.
        // Otherwise a later page could contain a larger ID at the same time.
        let window_complete = newest.len() == limit
            && newest
                .first_key_value()
                .is_some_and(|((oldest, _), _)| last_ts.is_some_and(|last| last < *oldest));
        if finished || lower_bound_reached || window_complete {
            return Ok(Some(newest));
        }
        if work >= budget {
            return Ok(None);
        }
        ceiling = last_ts;
        skip_at_ceiling = trailing_count;
    }
}

/// The agent's next receipt page after `after`, ascending by message ID. An
/// unordered page shorter than `PAGE_SIZE` already holds every remaining
/// receipt. A full one is used only when the index count over its ID range
/// equals its size, i.e. it is exactly the lowest remaining IDs; under a plan
/// that returns rows in any other order the page is re-read with ORDER BY.
fn receipt_page(
    conn: &impl SyncQuery,
    agent_id: i64,
    after: Option<i64>,
) -> Result<Vec<(i64, Row)>, DbError> {
    let (sql, count_sql, ordered_sql, mut params) = match after {
        None => (
            RECIPIENT_PAGE_SQL,
            RECIPIENT_PAGE_COUNT_SQL,
            RECIPIENT_PAGE_ORDERED_SQL,
            vec![Value::BigInt(agent_id)],
        ),
        Some(id) => (
            RECIPIENT_NEXT_SQL,
            RECIPIENT_NEXT_COUNT_SQL,
            RECIPIENT_NEXT_ORDERED_SQL,
            vec![Value::BigInt(agent_id), Value::BigInt(id)],
        ),
    };
    let page = keyed_by_message_id(query(conn, sql, &params)?)?;
    let Some(&(max, _)) = page.last().filter(|_| page.len() == PAGE_SIZE) else {
        return Ok(page);
    };
    params.push(Value::BigInt(max));
    let counted = query(conn, count_sql, &params)?
        .first()
        .map(|row| row.get_named::<i64>("n"))
        .transpose()
        .map_err(db_error)?;
    if counted.and_then(|n| usize::try_from(n).ok()) == Some(page.len()) {
        return Ok(page);
    }
    params.pop();
    keyed_by_message_id(query(conn, ordered_sql, &params)?)
}

fn keyed_by_message_id(rows: Vec<Row>) -> Result<Vec<(i64, Row)>, DbError> {
    if rows.len() > PAGE_SIZE {
        return Err(DbError::Internal(
            "inbox recipient page exceeded its bound".into(),
        ));
    }
    let mut page = rows
        .into_iter()
        .map(|row| Ok((row.get_named::<i64>("message_id").map_err(db_error)?, row)))
        .collect::<Result<Vec<_>, DbError>>()?;
    page.sort_unstable_by_key(|(id, _)| *id);
    Ok(page)
}

/// The newest window from every one of the agent's receipts.
fn receipt_window(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<NewestWindow, DbError> {
    let mut after = None;
    let mut newest = NewestWindow::new();
    // Receipts that passed the read/ACK filters, awaiting one header lookup
    // of up to PAGE_SIZE IDs: one statement per page costs more than the
    // rows it returns when most receipts are filtered out.
    let mut pending: HashMap<i64, Receipt> = HashMap::new();
    loop {
        let rows = receipt_page(conn, agent_id, after)?;
        let finished = rows.len() < PAGE_SIZE;
        for (id, row) in rows {
            if after.is_some_and(|previous| id <= previous) {
                return Err(DbError::Internal(
                    "inbox recipient cursor did not advance".into(),
                ));
            }
            after = Some(id);
            let receipt = Receipt {
                kind: row.get_named("kind").map_err(db_error)?,
                read_ts: row.get_named("read_ts").map_err(db_error)?,
                ack_ts: row.get_named("ack_ts").map_err(db_error)?,
            };
            if (options.unread_only && receipt.read_ts.is_some())
                || ((options.ack_required_only || options.ack_overdue_before.is_some())
                    && receipt.ack_ts.is_some())
            {
                continue;
            }
            pending.insert(id, receipt);
            if pending.len() == PAGE_SIZE {
                admit_headers(
                    conn,
                    project_id,
                    since_ts,
                    limit,
                    options,
                    &mut pending,
                    &mut newest,
                )?;
            }
        }
        if finished {
            break;
        }
    }
    admit_headers(
        conn,
        project_id,
        since_ts,
        limit,
        options,
        &mut pending,
        &mut newest,
    )?;
    Ok(newest)
}

/// Looks up the headers of the pending receipts, applies the header filters
/// and keeps the newest `limit`. Missing message rows are orphan recipients,
/// excluded by the old inner join too; their receipts are dropped.
fn admit_headers(
    conn: &impl SyncQuery,
    project_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
    pending: &mut HashMap<i64, Receipt>,
    newest: &mut NewestWindow,
) -> Result<(), DbError> {
    if pending.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = pending.keys().copied().collect();
    let (sql, params) = lookup_query("messages", HEADER_COLUMNS, &ids);
    for row in query(conn, &sql, &params)? {
        let id = row.get_named::<i64>("id").map_err(db_error)?;
        let receipt = pending.remove(&id).ok_or_else(|| {
            DbError::Internal("message lookup returned an unrequested inbox ID".into())
        })?;
        if row.get_named::<i64>("project_id").map_err(db_error)? != project_id {
            continue;
        }
        let created = row.get_named::<i64>("created_ts").map_err(db_error)?;
        let ack_required = row.get_named::<i64>("ack_required").map_err(db_error)?;
        let importance = row.get_named::<String>("importance").map_err(db_error)?;
        let topic = row.get_named::<Option<String>>("topic").map_err(db_error)?;
        if (options.urgent_only && !matches!(importance.as_str(), "high" | "urgent"))
            || (options.ack_required_only && ack_required != 1)
            || options
                .ack_overdue_before
                .is_some_and(|ts| ack_required != 1 || created >= ts)
            || since_ts.is_some_and(|ts| created <= ts)
            || options.topic.is_some_and(|wanted| {
                !topic
                    .as_deref()
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
            })
        {
            continue;
        }
        let key = (created, id);
        if newest.len() < limit || newest.first_key_value().is_some_and(|(old, _)| key > *old) {
            newest.insert(key, receipt);
            if newest.len() > limit {
                newest.pop_first();
            }
        }
    }
    pending.clear();
    Ok(())
}

fn hydrate(
    conn: &impl SyncQuery,
    project_id: i64,
    newest: BTreeMap<(i64, i64), Receipt>,
    body_policy: InboxBodyPolicy,
    candidates_only: bool,
) -> Result<Vec<InboxRow>, DbError> {
    let expected = newest.len();
    let ids: Vec<i64> = newest.keys().rev().map(|(_, id)| *id).collect();
    let mut selected: HashMap<i64, (i64, Receipt)> = newest
        .into_iter()
        .map(|((created, id), receipt)| (id, (created, receipt)))
        .collect();
    let body = match body_policy {
        InboxBodyPolicy::Full => "body_md",
        InboxBodyPolicy::MetadataOnly => "'' AS body_md",
    };
    let columns = if candidates_only {
        "id, project_id, sender_id, NULL AS thread_id, NULL AS topic, '' AS subject, \
         '' AS body_md, importance, ack_required, created_ts, '[]' AS recipients_json, \
         '[]' AS attachments"
            .to_string()
    } else {
        format!(
            "id, project_id, sender_id, thread_id, topic, subject, {body}, \
             importance, ack_required, created_ts, recipients_json, attachments"
        )
    };
    let mut out = Vec::with_capacity(expected);
    for chunk in ids.chunks(PAGE_SIZE) {
        let (sql, params) = lookup_query("messages", &columns, chunk);
        for row in query(conn, &sql, &params)? {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let (created, receipt) = selected.remove(&id).ok_or_else(|| {
                DbError::Internal("inbox hydration returned an unselected ID".into())
            })?;
            let message = MessageRow {
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
            if message.project_id != project_id || message.created_ts != created {
                return Err(DbError::Internal(
                    "inbox source changed inside its read snapshot".into(),
                ));
            }
            out.push(InboxRow {
                message,
                kind: receipt.kind,
                sender_name: UNKNOWN_SENDER_DISPLAY.to_string(),
                read_ts: receipt.read_ts,
                ack_ts: receipt.ack_ts,
            });
        }
    }
    if out.len() != expected {
        return Err(DbError::Internal(
            "selected inbox message disappeared inside its read snapshot".into(),
        ));
    }
    if candidates_only {
        out.sort_unstable_by_key(|row| std::cmp::Reverse((row.message.created_ts, row.message.id)));
        return Ok(out);
    }
    let mut senders: Vec<i64> = out.iter().map(|row| row.message.sender_id).collect();
    senders.sort_unstable();
    senders.dedup();
    let mut names = HashMap::new();
    for chunk in senders.chunks(PAGE_SIZE) {
        let (sql, params) = lookup_query("agents", "id, name", chunk);
        for row in query(conn, &sql, &params)? {
            names.insert(
                row.get_named::<i64>("id").map_err(db_error)?,
                row.get_named::<String>("name").map_err(db_error)?,
            );
        }
    }
    for row in &mut out {
        if let Some(name) = names.get(&row.message.sender_id) {
            row.sender_name.clone_from(name);
        }
    }
    out.sort_unstable_by_key(|row| std::cmp::Reverse((row.message.created_ts, row.message.id)));
    Ok(out)
}

/// Materialize only the final priority window after its byte preflight.
pub(super) fn hydrate_priority_rows(
    conn: &impl SyncQuery,
    project_id: i64,
    candidates: Vec<InboxRow>,
    body_policy: InboxBodyPolicy,
) -> Result<Vec<InboxRow>, DbError> {
    let mut newest = NewestWindow::new();
    for row in candidates {
        let id = row
            .message
            .id
            .ok_or_else(|| DbError::Internal("priority candidate has no ID".into()))?;
        newest.insert(
            (row.message.created_ts, id),
            Receipt {
                kind: row.kind,
                read_ts: row.read_ts,
                ack_ts: row.ack_ts,
            },
        );
    }
    hydrate(conn, project_id, newest, body_policy, false)
}

const SHARED_PAGE_SIZE: usize = 128;

/// Page authoritative message timestamps, not the denormalized delivery time.
/// Ordering on a second column disables the pinned engine's timestamp index.
/// Within the same read snapshot, skip only the already visited part of the
/// boundary timestamp's index run. Never assume IDs track creation time or
/// that equal-timestamp rows arrive in any particular ID order.
#[must_use]
pub fn shared_page_query(
    project_id: i64,
    ceiling: Option<i64>,
    skip_at_ceiling: i64,
) -> (String, Vec<Value>) {
    let mut sql = format!("SELECT {HEADER_COLUMNS} FROM messages WHERE project_id = ?");
    let mut params = vec![Value::BigInt(project_id)];
    if let Some(ts) = ceiling {
        sql.push_str(" AND created_ts <= ?");
        params.push(Value::BigInt(ts));
    }
    sql.push_str(" ORDER BY created_ts DESC LIMIT 128 OFFSET ?");
    params.push(Value::BigInt(skip_at_ceiling));
    (sql, params)
}

fn shared_lookup_query(
    table: &str,
    columns: &str,
    ids: &[i64],
    agent_id: Option<i64>,
) -> (String, Vec<Value>) {
    debug_assert!(!ids.is_empty() && ids.len() <= SHARED_PAGE_SIZE);
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut sql = format!("SELECT {columns} FROM {table} WHERE message_id IN ({placeholders})");
    let mut params: Vec<Value> = ids.iter().copied().map(Value::BigInt).collect();
    if let Some(agent_id) = agent_id {
        sql.push_str(" AND agent_id = ?");
        params.push(Value::BigInt(agent_id));
    }
    (sql, params)
}

/// Bound composite IN keys scan the pinned engine's entire receipt table;
/// large literal lists also lose the composite-index path. Small batches of
/// typed i64 keys preserve indexed lookups without interpolating user text.
fn shared_receipt_rows(
    conn: &impl SyncQuery,
    table: &str,
    columns: &str,
    ids: &[i64],
    agent_id: i64,
) -> Result<Vec<Row>, DbError> {
    let mut out = Vec::new();
    for chunk in ids.chunks(8) {
        let keys = chunk
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT {columns} FROM {table} WHERE message_id IN ({keys}) AND agent_id = ?");
        let rows = query(conn, &sql, &[Value::BigInt(agent_id)])?;
        if rows.len() > chunk.len() {
            return Err(DbError::Internal(
                "shared inbox receipt batch returned duplicate keys".into(),
            ));
        }
        out.extend(rows);
    }
    Ok(out)
}

/// The public shared-mailbox reader checks schema authority and supplies a
/// read snapshot. All joins here are bounded primary-key probes. Sparse or
/// heavily filtered mail may inspect more candidates; no fixed scan budget
/// is allowed to turn unseen mail into a successful empty inbox.
pub fn fetch_shared(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_shared_with_policy(conn, project_id, agent_id, since_ts, limit, options, false)
}

pub(crate) fn fetch_shared_with_policy(
    conn: &impl SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
    candidates_only: bool,
) -> Result<Vec<InboxRow>, DbError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let viewer = query(
        conn,
        "SELECT project_id, inception_ts, retired_at, contact_policy FROM agents WHERE id = ?",
        &[Value::BigInt(agent_id)],
    )?;
    let Some(viewer) = viewer.first() else {
        return Ok(Vec::new());
    };
    if viewer.get_named::<i64>("project_id").map_err(db_error)? != project_id
        || viewer
            .get_named::<Option<i64>>("retired_at")
            .map_err(db_error)?
            .is_some()
        || viewer
            .get_named::<Option<String>>("contact_policy")
            .map_err(db_error)?
            .is_some_and(|policy| policy.eq_ignore_ascii_case("block_all"))
    {
        return Ok(Vec::new());
    }
    let inception = viewer.get_named::<i64>("inception_ts").map_err(db_error)?;
    let mut newest: BTreeMap<(i64, i64), Receipt> = BTreeMap::new();
    let mut ceiling = None;
    let mut skip_at_ceiling = 0_i64;
    loop {
        let (sql, params) = shared_page_query(project_id, ceiling, skip_at_ceiling);
        let rows = query(conn, &sql, &params)?;
        if rows.len() > SHARED_PAGE_SIZE {
            return Err(DbError::Internal(
                "shared inbox page exceeded its bound".into(),
            ));
        }
        let finished = rows.len() < SHARED_PAGE_SIZE;
        let mut last_ts = ceiling;
        let mut trailing_count = skip_at_ceiling;
        let mut headers = HashMap::new();
        let mut lower_bound_reached = false;
        for row in rows {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let created = row.get_named::<i64>("created_ts").map_err(db_error)?;
            if last_ts.is_some_and(|previous| created > previous) {
                return Err(DbError::Internal(
                    "shared inbox timestamps are not descending".into(),
                ));
            }
            if last_ts == Some(created) {
                trailing_count = trailing_count.checked_add(1).ok_or_else(|| {
                    DbError::Internal("shared inbox timestamp offset overflow".into())
                })?;
            } else {
                last_ts = Some(created);
                trailing_count = 1;
            }
            if created < inception || since_ts.is_some_and(|ts| created <= ts) {
                lower_bound_reached = true;
                continue;
            }
            if row.get_named::<i64>("project_id").map_err(db_error)? != project_id {
                return Err(DbError::Internal(
                    "shared inbox returned a foreign project header".into(),
                ));
            }
            if row.get_named::<i64>("sender_id").map_err(db_error)? == agent_id {
                continue;
            }
            let ack = row.get_named::<i64>("ack_required").map_err(db_error)?;
            let importance = row.get_named::<String>("importance").map_err(db_error)?;
            let topic = row.get_named::<Option<String>>("topic").map_err(db_error)?;
            if (options.urgent_only && !matches!(importance.as_str(), "high" | "urgent"))
                || (options.ack_required_only && ack != 1)
                || options
                    .ack_overdue_before
                    .is_some_and(|ts| ack != 1 || created >= ts)
                || options.topic.is_some_and(|wanted| {
                    !topic
                        .as_deref()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
                })
            {
                continue;
            }
            if headers.insert(id, created).is_some() {
                return Err(DbError::Internal("duplicate shared inbox candidate".into()));
            }
        }
        if !headers.is_empty() {
            let ids: Vec<i64> = headers.keys().copied().collect();
            let (sql, params) = shared_lookup_query(
                "project_mailbox_deliveries",
                "message_id, project_id",
                &ids,
                None,
            );
            let mut deliveries = HashMap::new();
            for row in query(conn, &sql, &params)? {
                let id = row.get_named::<i64>("message_id").map_err(db_error)?;
                let created = headers
                    .get(&id)
                    .ok_or_else(|| DbError::Internal("unrequested shared inbox delivery".into()))?;
                if row.get_named::<i64>("project_id").map_err(db_error)? == project_id {
                    deliveries.insert(id, *created);
                }
            }
            if !deliveries.is_empty() {
                let ids: Vec<i64> = deliveries.keys().copied().collect();
                for row in
                    shared_receipt_rows(conn, "message_recipients", "message_id", &ids, agent_id)?
                {
                    deliveries.remove(&row.get_named::<i64>("message_id").map_err(db_error)?);
                }
                if !deliveries.is_empty() {
                    let ids: Vec<i64> = deliveries.keys().copied().collect();
                    let mut receipts = HashMap::new();
                    for row in shared_receipt_rows(
                        conn,
                        "project_mailbox_receipts",
                        "message_id, read_ts, ack_ts",
                        &ids,
                        agent_id,
                    )? {
                        let id = row.get_named::<i64>("message_id").map_err(db_error)?;
                        if !deliveries.contains_key(&id) {
                            return Err(DbError::Internal(
                                "unrequested shared inbox receipt".into(),
                            ));
                        }
                        let read_ts = row.get_named::<Option<i64>>("read_ts").map_err(db_error)?;
                        let ack_ts = row.get_named::<Option<i64>>("ack_ts").map_err(db_error)?;
                        receipts.insert(id, (read_ts, ack_ts));
                    }
                    for (id, created) in deliveries {
                        let (read_ts, ack_ts) = receipts.remove(&id).unwrap_or((None, None));
                        if (options.unread_only && read_ts.is_some())
                            || ((options.ack_required_only || options.ack_overdue_before.is_some())
                                && ack_ts.is_some())
                        {
                            continue;
                        }
                        let key = (created, id);
                        if newest.len() < limit
                            || newest.first_key_value().is_some_and(|(old, _)| key > *old)
                        {
                            newest.insert(
                                key,
                                Receipt {
                                    kind: crate::project_mailbox::PROJECT_MAILBOX_KIND.into(),
                                    read_ts,
                                    ack_ts,
                                },
                            );
                            if newest.len() > limit {
                                newest.pop_first();
                            }
                        }
                    }
                }
            }
        }
        // Finish the boundary timestamp even after collecting `limit` rows.
        // Otherwise a later page could contain a larger ID at the same time.
        let window_complete = newest.len() == limit
            && newest
                .first_key_value()
                .is_some_and(|((oldest, _), _)| last_ts.is_some_and(|last| last < *oldest));
        if finished || lower_bound_reached || window_complete {
            break;
        }
        ceiling = last_ts;
        skip_at_ceiling = trailing_count;
    }
    hydrate(
        conn,
        project_id,
        newest,
        options.body_policy,
        candidates_only,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DbConn;
    use crate::sync::{fetch_inbox_rows_from_conn_impl, reference_inbox_rows_from_conn_impl};
    use std::cell::RefCell;

    fn options() -> InboxFetchOptions<'static> {
        InboxFetchOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::Full,
        }
    }

    fn seed(count: i64) -> DbConn {
        let conn = super::super::tests::test_conn();
        conn.execute_raw("INSERT INTO projects(id,slug,human_key,created_at) VALUES \
            (1,'one','/one',0),(2,'two','/two',0); \
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
            (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',0,0), \
            (3,1,'RedFox','test','test',0,0),(4,2,'GoldFox','test','test',0,0); \
            BEGIN").unwrap();
        for id in 1..=count {
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts, \
                    importance,ack_required,thread_id,topic) VALUES \
                 ({id},1,1,'subject-{id}','body-{id}',{},'{}',{},'thread','{}'); \
                 INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) \
                 VALUES({id},2,'{}',{},{})",
                1_000_000 + ((id * 17) % (count + 1)) * 1000,
                ["normal", "high", "urgent", "low"][usize::try_from(id % 4).unwrap()],
                i64::from(id % 3 != 0),
                ["Handoffs", "other", "é", "É"][usize::try_from(id % 4).unwrap()],
                ["to", "cc", "bcc"][usize::try_from(id % 3).unwrap()],
                if id % 5 == 0 { "2000000" } else { "NULL" },
                if id % 7 == 0 { "2500000" } else { "NULL" },
            ))
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        conn
    }

    fn same_rows(actual: &[InboxRow], expected: &[InboxRow]) {
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }

    #[test]
    fn indexed_inbox_matches_reference_for_all_filters_and_body_policies() {
        let conn = seed(32);
        for flags in 0..8 {
            for since in [None, Some(1_016_000)] {
                for overdue in [None, Some(1_024_000)] {
                    for topic in [None, Some("HANDOFFS"), Some("É"), Some("missing")] {
                        for limit in [0, 1, 7, 100] {
                            for body_policy in
                                [InboxBodyPolicy::Full, InboxBodyPolicy::MetadataOnly]
                            {
                                let filters = InboxFetchOptions {
                                    urgent_only: flags & 1 != 0,
                                    unread_only: flags & 2 != 0,
                                    ack_required_only: flags & 4 != 0,
                                    ack_overdue_before: overdue,
                                    topic,
                                    body_policy,
                                };
                                let expected = reference_inbox_rows_from_conn_impl(
                                    &conn, 1, 2, since, limit, filters,
                                )
                                .unwrap();
                                let actual = fetch_inbox_rows_from_conn_impl(
                                    &conn, 1, 2, since, limit, filters,
                                )
                                .unwrap();
                                same_rows(&actual, &expected);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn filtered_pages_orphans_and_foreign_messages_do_not_hide_later_mail() {
        let conn = seed(600);
        conn.execute_raw("UPDATE message_recipients SET read_ts = 1 WHERE message_id <= 256; \
            UPDATE messages SET project_id = 2 WHERE id = 590; \
            PRAGMA foreign_keys = OFF; \
            INSERT INTO message_recipients(message_id,agent_id,kind) VALUES (0,2,'to'),(10000,2,'to'); \
            UPDATE messages SET sender_id = 99999 WHERE id = 599").unwrap();
        let filters = InboxFetchOptions {
            unread_only: true,
            ..options()
        };
        let expected =
            reference_inbox_rows_from_conn_impl(&conn, 1, 2, None, 500, filters).unwrap();
        let actual = fetch_inbox_rows_from_conn_impl(&conn, 1, 2, None, 500, filters).unwrap();
        same_rows(&actual, &expected);
        assert!(actual.len() > PAGE_SIZE);
        assert!(actual.iter().any(|row| row.message.id == Some(599)
            && row.sender_name == UNKNOWN_SENDER_DISPLAY));
        assert!(actual.iter().all(|row| row.message.project_id == 1));
    }

    #[test]
    fn equal_timestamps_use_message_id_for_a_stable_newest_window() {
        let conn = seed(300);
        conn.execute_raw("UPDATE messages SET created_ts = 1000000")
            .unwrap();
        let actual = fetch_inbox_rows_from_conn_impl(&conn, 1, 2, None, 20, options()).unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|row| row.message.id.unwrap())
                .collect::<Vec<_>>(),
            (281..=300).rev().collect::<Vec<_>>()
        );
    }

    struct Recording<'a> {
        conn: &'a DbConn,
        calls: RefCell<Vec<(String, usize, usize)>>,
    }

    impl SyncQuery for Recording<'_> {
        fn query_sync(
            &self,
            sql: &str,
            params: &[Value],
        ) -> Result<Vec<Row>, sqlmodel_core::Error> {
            let rows = self.conn.query_sync(sql, params)?;
            self.calls
                .borrow_mut()
                .push((sql.to_string(), params.len(), rows.len()));
            Ok(rows)
        }
        fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
            self.conn.execute_raw(sql)
        }
    }

    #[test]
    fn indexed_pipeline_bounds_each_lookup_and_hydrates_only_selected_bodies() {
        let conn = seed(600);
        // Pin the receipt scan this test bounds: without a count the
        // newest-first walk is not tried (its bounds are asserted below).
        conn.execute_raw("DELETE FROM inbox_stats").unwrap();
        let recording = Recording {
            conn: &conn,
            calls: RefCell::new(Vec::new()),
        };
        let result = with_snapshot(&recording, || {
            fetch_direct(&recording, 1, 2, None, 7, options())
        })
        .unwrap();
        assert_eq!(result.len(), 7);
        let calls = recording.calls.borrow();
        assert!(calls.iter().all(|(sql, binds, rows)| !sql.contains("JOIN")
            && *binds <= PAGE_SIZE
            && *rows <= PAGE_SIZE));
        assert_eq!(
            calls
                .iter()
                .filter(|(sql, _, _)| sql.contains("body_md"))
                .map(|(_, _, rows)| rows)
                .sum::<usize>(),
            7
        );
        // Every receipt read once; the full-page index counts return one row each.
        assert_eq!(
            calls
                .iter()
                .filter(|(sql, _, _)| sql.contains("FROM message_recipients")
                    && !sql.contains("COUNT(*)"))
                .map(|(_, _, rows)| rows)
                .sum::<usize>(),
            600
        );
    }

    #[test]
    fn read_snapshot_preserves_callers_transaction_and_query_only_mode() {
        let conn = seed(5);
        conn.execute_raw("BEGIN; UPDATE messages SET subject = 'uncommitted' WHERE id = 1")
            .unwrap();
        let actual = fetch_inbox_rows_from_conn_impl(&conn, 1, 2, None, 10, options()).unwrap();
        assert!(
            actual
                .iter()
                .any(|row| row.message.subject == "uncommitted")
        );
        assert!(
            with_snapshot(&conn, || -> Result<(), DbError> {
                Err(DbError::Internal("injected read failure".into()))
            })
            .is_err()
        );
        conn.execute_raw("ROLLBACK").unwrap();
        assert_eq!(
            conn.query_sync("SELECT subject FROM messages WHERE id = 1", &[])
                .unwrap()[0]
                .get_named::<String>("subject")
                .unwrap(),
            "subject-1"
        );
        conn.execute_raw("PRAGMA query_only = ON").unwrap();
        assert_eq!(
            fetch_inbox_rows_from_conn_impl(&conn, 1, 2, None, 10, options())
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn file_backed_inbox_keeps_one_snapshot_across_a_peer_commit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox-snapshot.sqlite3");
        let reader = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "inbox snapshot reader",
        );
        reader
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .unwrap();
        let cx = asupersync::Cx::for_testing();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            crate::schema::migrate_to_latest_base(&cx, &*reader)
                .await
                .into_result()
                .unwrap();
        });
        reader.execute_raw("INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'one','/one',0); \
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
            (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',0,0); \
            INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts) \
            VALUES(1,1,1,'old subject','old body',1000); \
            INSERT INTO message_recipients(message_id,agent_id,kind) VALUES(1,2,'to')").unwrap();
        let writer = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "inbox snapshot peer",
        );
        struct PeerCommit<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            committed: std::cell::Cell<bool>,
        }
        impl SyncQuery for PeerCommit<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql == RECIPIENT_PAGE_SQL && !self.committed.replace(true) {
                    self.writer.execute_raw("BEGIN IMMEDIATE; \
                        UPDATE messages SET subject = 'new subject', body_md = 'new body' WHERE id = 1; \
                        UPDATE message_recipients SET read_ts = 2000, ack_ts = 3000 WHERE message_id = 1; \
                        COMMIT")?;
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
        }
        let racing = PeerCommit {
            reader: &reader,
            writer: &writer,
            committed: std::cell::Cell::new(false),
        };
        let before = fetch_inbox_rows_from_conn_impl(&racing, 1, 2, None, 20, options()).unwrap();
        assert!(
            racing.committed.get(),
            "a real peer transaction must commit between the read stages"
        );
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].message.subject, "old subject");
        assert_eq!(before[0].message.body_md, "old body");
        assert_eq!((before[0].read_ts, before[0].ack_ts), (None, None));
        let after = fetch_inbox_rows_from_conn_impl(&*reader, 1, 2, None, 20, options()).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message.subject, "new subject");
        assert_eq!(after[0].message.body_md, "new body");
        assert_eq!(
            (after[0].read_ts, after[0].ack_ts),
            (Some(2000), Some(3000))
        );
    }

    /// `(opcode, p2, p3)` of every instruction in the statement's program.
    fn explain_ops(conn: &DbConn, sql: &str, params: &[Value]) -> Vec<(String, i64, i64)> {
        conn.query_sync(&format!("EXPLAIN {sql}"), params)
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.get_as::<String>(1).unwrap(),
                    row.get_as::<i64>(3).unwrap_or(0),
                    row.get_as::<i64>(4).unwrap_or(0),
                )
            })
            .collect()
    }

    /// The program seeks once with a key of at least `fields` columns and
    /// walks a table or index from one end only on the engine's fallback for a
    /// key value that is not an integer, which sits behind an `IsType` check.
    fn assert_bounded_seek(conn: &DbConn, sql: &str, params: &[Value], fields: i64) {
        let ops = explain_ops(conn, sql, params);
        let seeks: Vec<i64> = ops
            .iter()
            .filter(|(op, _, _)| op == "SeekGE")
            .map(|(_, _, key)| *key)
            .collect();
        assert_eq!(seeks.len(), 1, "{sql}: {ops:?}");
        assert!(
            ops.iter()
                .any(|(op, n, out)| op == "MakeRecord" && *out == seeks[0] && *n >= fields),
            "{sql} must seek on {fields} index columns: {ops:?}"
        );
        let guard = ops.iter().position(|(op, _, _)| op == "IsType");
        assert!(
            ops.iter().enumerate().all(|(at, (op, _, _))| {
                !matches!(op.as_str(), "Rewind" | "Last") || guard.is_some_and(|g| g < at)
            }),
            "{sql} must not walk a history on its integer-key path: {ops:?}"
        );
    }

    #[test]
    fn indexed_queries_use_recipient_range_and_message_primary_keys() {
        let conn = seed(5);
        // Plan labels read SCAN for these unordered seeks; check the programs.
        let (agent, after, max) = (Value::BigInt(2), Value::BigInt(0), Value::BigInt(9));
        assert_bounded_seek(&conn, RECIPIENT_PAGE_SQL, std::slice::from_ref(&agent), 1);
        assert_bounded_seek(
            &conn,
            RECIPIENT_NEXT_SQL,
            &[agent.clone(), after.clone()],
            2,
        );
        assert_bounded_seek(
            &conn,
            RECIPIENT_PAGE_COUNT_SQL,
            &[agent.clone(), max.clone()],
            1,
        );
        assert_bounded_seek(&conn, RECIPIENT_NEXT_COUNT_SQL, &[agent, after, max], 2);
        let (sql, params) = lookup_query("messages", HEADER_COLUMNS, &[1, 2, 3]);
        let rows = conn
            .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), &params)
            .unwrap();
        let details = rows
            .iter()
            .map(|row| row.get_as::<String>(3).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains("PRIMARY KEY"), "{details}");
        assert!(!details.contains("SCAN messages"), "{details}");
        assert!(RECIPIENT_RANGE_SQL.ends_with(&format!("LIMIT {RECIPIENT_RANGE_LIMIT}")));
        // The plan label cannot tell a two-column seek from an agent_id-only
        // seek that walks the inbox, and without ORDER BY it even reads SCAN.
        // It still names the index; the program decides the rest: one SeekGE
        // whose key record holds both columns, and no Rewind/Last history walk.
        let plan = conn
            .query_sync(
                &format!("EXPLAIN QUERY PLAN {RECIPIENT_RANGE_SQL}"),
                &[2_i64.into(), 1_i64.into(), 5_i64.into()],
            )
            .unwrap()
            .iter()
            .map(|row| row.get_as::<String>(3).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plan.contains("USING INDEX idx_message_recipients_agent_message"),
            "{plan}"
        );
        assert_bounded_seek(
            &conn,
            RECIPIENT_RANGE_SQL,
            &[2_i64.into(), 1_i64.into(), 5_i64.into()],
            2,
        );
        let hint = conn
            .query_sync(
                &format!("EXPLAIN QUERY PLAN {INBOX_STATS_HINT_SQL}"),
                &[2_i64.into()],
            )
            .unwrap()
            .iter()
            .map(|row| row.get_as::<String>(3).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            hint.contains("SEARCH inbox_stats USING INTEGER PRIMARY KEY"),
            "{hint}"
        );
    }

    pub(super) fn walk_conn() -> DbConn {
        let conn = super::super::tests::test_conn();
        conn.execute_raw("INSERT INTO projects(id,slug,human_key,created_at) VALUES \
            (1,'one','/one',0),(2,'two','/two',0); \
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
            (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',0,0), \
            (3,1,'RedFox','test','test',0,0),(4,2,'GoldFox','test','test',0,0)").unwrap();
        conn
    }

    /// Messages `first..=last` in project 1 from BlueLake. Every `every`-th is
    /// GreenStone's (agent 2) and the rest RedFox's, so other mail interleaves
    /// the inbox. Receipt state varies like [`seed`]; agent 2's `inbox_stats`
    /// row is rebuilt afterwards.
    pub(super) fn seed_walk(
        conn: &DbConn,
        (first, last): (i64, i64),
        every: i64,
        created: fn(i64) -> i64,
        importance: fn(i64) -> &'static str,
    ) {
        conn.execute_raw("BEGIN").unwrap();
        for start in (first..=last).step_by(128) {
            let end = (start + 127).min(last);
            let messages = (start..=end)
                .map(|id| {
                    format!(
                        "({id},1,1,'subject-{id}','body-{id}',{},'{}',{},'thread','{}')",
                        created(id),
                        importance(id),
                        i64::from(id % 3 != 0),
                        ["Handoffs", "other", "é", "É"][usize::try_from(id % 4).unwrap()],
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let receipts = (start..=end)
                .map(|id| {
                    format!(
                        "({id},{},'{}',{},{})",
                        if id % every == 0 { 2 } else { 3 },
                        ["to", "cc", "bcc"][usize::try_from(id % 3).unwrap()],
                        if id % 5 == 0 { "2000000" } else { "NULL" },
                        if id % 7 == 0 { "2500000" } else { "NULL" },
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts, \
                    importance,ack_required,thread_id,topic) VALUES {messages}; \
                 INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) \
                    VALUES {receipts}"
            ))
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        super::super::rebuild_agent_inbox_stats_sync(conn, 2).unwrap();
    }

    /// A fixture's `created_ts` for a message ID.
    type Created = fn(i64) -> i64;

    fn mixed_importance(id: i64) -> &'static str {
        ["normal", "high", "urgent", "low"][usize::try_from((id / 2) % 4).unwrap()]
    }

    /// The reference query orders by `created_ts` alone, so the engine breaks
    /// timestamp ties in any order. The defined window is newest
    /// `(created_ts, id)` first: take the whole filtered inbox and cut it.
    fn newest_reference(
        conn: &DbConn,
        since: Option<i64>,
        limit: usize,
        filters: InboxFetchOptions<'_>,
    ) -> Vec<InboxRow> {
        let mut rows =
            reference_inbox_rows_from_conn_impl(conn, 1, 2, since, 1_000_000, filters).unwrap();
        rows.sort_by_key(|row| std::cmp::Reverse((row.message.created_ts, row.message.id)));
        rows.truncate(limit);
        rows
    }

    fn query_counts(calls: &[(String, usize, usize)]) -> (usize, usize, usize) {
        let count =
            |wanted: &dyn Fn(&str) -> bool| calls.iter().filter(|(sql, _, _)| wanted(sql)).count();
        (
            count(&|sql| sql.contains("ORDER BY created_ts DESC LIMIT 128")),
            count(&|sql| sql == RECIPIENT_PAGE_SQL || sql == RECIPIENT_NEXT_SQL),
            count(&|sql| sql.contains("FROM message_recipients WHERE message_id IN (")),
        )
    }

    #[test]
    fn newest_first_walk_matches_reference_on_dense_scrambled_and_tied_inboxes() {
        let fixtures: [(&str, Created); 3] = [
            ("monotone", |id| 1_000_000 + id * 1000),
            // Message IDs that do not follow creation time.
            ("scrambled", |id| 1_000_000 + ((id * 17) % 1025) * 1000),
            // Equal-timestamp runs longer than a header page.
            ("tied", |id| 1_000_000 + (id / 300) * 1000),
        ];
        for (name, created) in fixtures {
            let conn = walk_conn();
            seed_walk(&conn, (1, 1024), 2, created, mixed_importance);
            conn.execute_raw(
                "UPDATE messages SET project_id = 2 WHERE id = 1000; \
                PRAGMA foreign_keys = OFF; \
                INSERT INTO message_recipients(message_id,agent_id,kind) VALUES (50000,2,'to')",
            )
            .unwrap();
            for urgent_only in [false, true] {
                for since in [None, Some(created(700))] {
                    for topic in [None, Some("HANDOFFS")] {
                        for limit in [1, 20, 100] {
                            let filters = InboxFetchOptions {
                                urgent_only,
                                topic,
                                ..options()
                            };
                            let expected = newest_reference(&conn, since, limit, filters);
                            let actual =
                                fetch_inbox_rows_from_conn_impl(&conn, 1, 2, since, limit, filters)
                                    .unwrap();
                            same_rows(&actual, &expected);
                        }
                    }
                }
            }
            // The walk, not the receipt scan, served the default window.
            let recording = Recording {
                conn: &conn,
                calls: RefCell::new(Vec::new()),
            };
            let rows = with_snapshot(&recording, || {
                fetch_direct(&recording, 1, 2, None, 20, options())
            })
            .unwrap();
            assert_eq!(rows.len(), 20, "{name}");
            same_rows(&rows, &newest_reference(&conn, None, 20, options()));
            assert!(
                recording
                    .calls
                    .borrow()
                    .iter()
                    .all(|(sql, binds, rows)| !sql.contains("JOIN")
                        && *binds <= PAGE_SIZE
                        && *rows <= PAGE_SIZE),
                "{name}: every walk lookup stays bounded"
            );
            let (pages, receipt_pages, batches) = query_counts(&recording.calls.borrow());
            assert!(pages >= 1, "{name}: no header page was read");
            assert_eq!(receipt_pages, 0, "{name}: the receipt scan ran");
            if name == "scrambled" {
                assert!(batches > 0, "{name}: wide ID ranges must use key batches");
            } else {
                assert_eq!(batches, 0, "{name}: one range read covers a page");
            }
        }
    }

    #[test]
    fn walk_cap_stale_or_missing_counts_and_state_filters_keep_exact_rows() {
        let conn = walk_conn();
        // GreenStone receives 1 in 40: a 20-row window needs ~800 headers.
        seed_walk(
            &conn,
            (1, 4096),
            40,
            |id| 1_000_000 + id * 1000,
            mixed_importance,
        );
        let run = |limit, filters: InboxFetchOptions<'_>| {
            let recording = Recording {
                conn: &conn,
                calls: RefCell::new(Vec::new()),
            };
            let actual = with_snapshot(&recording, || {
                fetch_direct(&recording, 1, 2, None, limit, filters)
            })
            .unwrap();
            let expected = newest_reference(&conn, None, limit, filters);
            same_rows(&actual, &expected);
            (expected.len(), query_counts(&recording.calls.borrow()))
        };
        // An understated count caps the walk at 64 rows: one header page, then
        // the exact receipt scan. Returning the walk's partial window would
        // come back short of the reference.
        conn.execute_raw("UPDATE inbox_stats SET total_count = 256 WHERE agent_id = 2")
            .unwrap();
        for limit in [20, 60] {
            let (len, (pages, receipt_pages, _)) = run(limit, options());
            assert_eq!(len, limit);
            assert_eq!((pages, receipt_pages > 0), (1, true), "limit {limit}");
        }
        // An overstated count lets the walk read the whole project history.
        conn.execute_raw("UPDATE inbox_stats SET total_count = 100000 WHERE agent_id = 2")
            .unwrap();
        let (len, (pages, receipt_pages, _)) = run(20, options());
        assert_eq!((len, pages, receipt_pages), (20, 7, 0));
        // Read/ACK-state filters keep the receipt scan whatever the count says.
        for filters in [
            InboxFetchOptions {
                unread_only: true,
                ..options()
            },
            InboxFetchOptions {
                ack_required_only: true,
                ..options()
            },
            InboxFetchOptions {
                ack_overdue_before: Some(3_000_000),
                ..options()
            },
        ] {
            let (_, (pages, receipt_pages, _)) = run(20, filters);
            assert_eq!((pages, receipt_pages > 0), (0, true));
        }
        // No count row, then no table: the receipt scan, without an error.
        conn.execute_raw("DELETE FROM inbox_stats WHERE agent_id = 2")
            .unwrap();
        let (_, (pages, _, _)) = run(20, options());
        assert_eq!(pages, 0);
        conn.execute_raw("DROP TABLE inbox_stats").unwrap();
        let (_, (pages, _, _)) = run(20, options());
        assert_eq!(pages, 0);
    }

    /// Stands in for a plan that ignores index order: unordered receipt pages
    /// come back as the highest 256 IDs after the cursor, newest first.
    struct HighestFirst<'a> {
        conn: &'a DbConn,
        ordered_rereads: std::cell::Cell<usize>,
    }

    impl SyncQuery for HighestFirst<'_> {
        fn query_sync(
            &self,
            sql: &str,
            params: &[Value],
        ) -> Result<Vec<Row>, sqlmodel_core::Error> {
            if sql == RECIPIENT_PAGE_SQL || sql == RECIPIENT_NEXT_SQL {
                let highest = sql.replace(" LIMIT 256", " ORDER BY message_id DESC LIMIT 256");
                return self.conn.query_sync(&highest, params);
            }
            if sql == RECIPIENT_PAGE_ORDERED_SQL || sql == RECIPIENT_NEXT_ORDERED_SQL {
                self.ordered_rereads.set(self.ordered_rereads.get() + 1);
            }
            self.conn.query_sync(sql, params)
        }
        fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
            self.conn.execute_raw(sql)
        }
    }

    #[test]
    fn receipt_pages_out_of_index_order_are_reread_in_order() {
        let conn = walk_conn();
        // 1,024 receipts: four full pages, then an empty one.
        seed_walk(
            &conn,
            (1, 2048),
            2,
            |id| 1_000_000 + id * 1000,
            mixed_importance,
        );
        for filters in [
            InboxFetchOptions {
                unread_only: true,
                ..options()
            },
            InboxFetchOptions {
                ack_required_only: true,
                ..options()
            },
            InboxFetchOptions {
                ack_overdue_before: Some(1_900_000),
                ..options()
            },
            InboxFetchOptions {
                unread_only: true,
                urgent_only: true,
                topic: Some("HANDOFFS"),
                ..options()
            },
        ] {
            for since in [None, Some(1_700_000)] {
                let expected = newest_reference(&conn, since, 1000, filters);
                // Trusting a full unordered page would stop after the highest
                // 256 receipts and come back short of the reference.
                let perturbed = HighestFirst {
                    conn: &conn,
                    ordered_rereads: std::cell::Cell::new(0),
                };
                let actual = with_snapshot(&perturbed, || {
                    fetch_direct(&perturbed, 1, 2, since, 1000, filters)
                })
                .unwrap();
                same_rows(&actual, &expected);
                assert_eq!(perturbed.ordered_rereads.get(), 3, "pages 1-3 are re-read");
                // An index-ordered plan never needs the ordered re-read.
                let recording = Recording {
                    conn: &conn,
                    calls: RefCell::new(Vec::new()),
                };
                let honest = with_snapshot(&recording, || {
                    fetch_direct(&recording, 1, 2, since, 1000, filters)
                })
                .unwrap();
                same_rows(&honest, &expected);
                assert!(recording.calls.borrow().iter().all(|(sql, _, _)| {
                    sql != RECIPIENT_PAGE_ORDERED_SQL && sql != RECIPIENT_NEXT_ORDERED_SQL
                }));
            }
        }
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::*;
    use crate::DbConn;
    use crate::sync::{fetch_inbox_rows_from_conn_impl, reference_inbox_rows_from_conn_impl};
    use std::time::Instant;

    #[test]
    #[ignore = "explicit same-invocation 2k/40k other-recipient scaling measurement"]
    fn inbox500_other2k_and40k_match_reference_with_bounded_scaling() {
        let conn = super::super::tests::test_conn();
        conn.execute_raw("INSERT INTO projects(id,slug,human_key,created_at) VALUES (1,'bench','/bench',0); \
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES \
            (1,1,'BlueLake','bench','bench',0,0),(2,1,'GreenStone','bench','bench',0,0), \
            (3,1,'RedFox','bench','bench',0,0)").unwrap();
        let filters = InboxFetchOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::Full,
        };
        let mut previous = 0;
        let mut candidate_medians = Vec::new();
        for total in [2500, 40500] {
            conn.execute_raw("BEGIN").unwrap();
            for id in previous + 1..=total {
                conn.execute_raw(&format!(
                    "INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts) \
                    VALUES({id},1,1,'message-{id}','body-{id}',{id}); \
                    INSERT INTO message_recipients(message_id,agent_id,kind) VALUES({id},{},'to')",
                    if id <= 500 { 2 } else { 3 }
                ))
                .unwrap();
            }
            conn.execute_raw("COMMIT").unwrap();
            previous = total;
            let mut candidate_times = Vec::new();
            let mut reference_times = Vec::new();
            for round in 0..5 {
                let candidate = || {
                    let start = Instant::now();
                    let rows =
                        fetch_inbox_rows_from_conn_impl(&conn, 1, 2, None, 20, filters).unwrap();
                    (rows, start.elapsed().as_micros())
                };
                let reference = || {
                    let start = Instant::now();
                    let rows = reference_inbox_rows_from_conn_impl(&conn, 1, 2, None, 20, filters)
                        .unwrap();
                    (rows, start.elapsed().as_micros())
                };
                let (actual, expected) = if round % 2 == 0 {
                    let actual = candidate();
                    (actual, reference())
                } else {
                    let expected = reference();
                    (candidate(), expected)
                };
                assert_eq!(format!("{:?}", actual.0), format!("{:?}", expected.0));
                candidate_times.push(actual.1);
                reference_times.push(expected.1);
            }
            candidate_times.sort_unstable();
            reference_times.sort_unstable();
            println!(
                "{{\"own_inbox\":500,\"other_messages\":{},\"candidate_median_us\":{},\"reference_median_us\":{},\"candidate_samples_us\":{:?},\"reference_samples_us\":{:?}}}",
                total - 500,
                candidate_times[2],
                reference_times[2],
                candidate_times,
                reference_times
            );
            candidate_medians.push(candidate_times[2]);
        }
        assert!(
            candidate_medians[1] <= 2 * candidate_medians[0],
            "unrelated delivery history still grows inbox cost: {candidate_medians:?}"
        );
    }

    /// Median microseconds of the newest-first plan and of the receipt scan
    /// alone, five alternating rounds, asserting identical rows every round.
    fn direct_plans_us(
        conn: &DbConn,
        limit: usize,
        filters: InboxFetchOptions<'_>,
    ) -> (u128, u128) {
        let mut candidate = Vec::new();
        let mut reference = Vec::new();
        for round in 0..5 {
            let new = || {
                let start = Instant::now();
                let rows =
                    with_snapshot(conn, || fetch_direct(conn, 1, 2, None, limit, filters)).unwrap();
                (rows, start.elapsed().as_micros())
            };
            let old = || {
                let start = Instant::now();
                let rows = with_snapshot(conn, || {
                    let window = receipt_window(conn, 1, 2, None, limit, filters)?;
                    hydrate(conn, 1, window, filters.body_policy)
                })
                .unwrap();
                (rows, start.elapsed().as_micros())
            };
            let (actual, expected) = if round % 2 == 0 {
                let actual = new();
                (actual, old())
            } else {
                let expected = old();
                (new(), expected)
            };
            assert_eq!(format!("{:?}", actual.0), format!("{:?}", expected.0));
            candidate.push(actual.1);
            reference.push(expected.1);
        }
        candidate.sort_unstable();
        reference.sort_unstable();
        (candidate[2], reference[2])
    }

    #[test]
    #[ignore = "explicit same-invocation direct inbox newest-first vs receipt-scan measurement (br-kp1in.40)"]
    fn direct_inbox_newest_first_vs_receipt_scan_dense_sparse_and_filtered() {
        use super::tests::{seed_walk, walk_conn};
        let filters = InboxFetchOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        };
        let created: fn(i64) -> i64 = |id| 1_000_000 + id * 1000;
        let report = |case: &str, own: i64, (new, old): (u128, u128)| {
            let ratio = new as f64 / old.max(1) as f64;
            println!(
                "{{\"case\":\"{case}\",\"own_inbox\":{own},\"newest_first_median_us\":{new},\"receipt_scan_median_us\":{old},\"ratio\":{ratio:.3}}}"
            );
            ratio
        };
        // M1/M2: GreenStone receives 1 in 4 of the project's mail.
        let conn = walk_conn();
        seed_walk(&conn, (1, 4_000), 4, created, |_| "normal");
        let small = direct_plans_us(&conn, 20, filters);
        report("dense", 1_000, small);
        seed_walk(&conn, (4_001, 40_000), 4, created, |_| "normal");
        let large = direct_plans_us(&conn, 20, filters);
        let m1 = report("dense", 10_000, large);
        // G1: 1 in 100, so a 20-row window needs ~2,000 headers; the cap is 250.
        let conn = walk_conn();
        seed_walk(&conn, (1, 100_000), 100, created, |_| "normal");
        let g1 = report("sparse", 1_000, direct_plans_us(&conn, 20, filters));
        // G2': 1 in 50 of GreenStone's mail is urgent; urgent_only exhausts the cap.
        let conn = walk_conn();
        seed_walk(&conn, (1, 8_000), 4, created, |id| {
            if (id / 4) % 50 == 0 {
                "urgent"
            } else {
                "normal"
            }
        });
        let urgent = InboxFetchOptions {
            urgent_only: true,
            ..filters
        };
        let g2 = report("urgent_rare", 2_000, direct_plans_us(&conn, 20, urgent));
        assert!(
            m1 <= 0.50,
            "M1 dense 10k newest-first/receipt-scan {m1:.3} > 0.50"
        );
        // Pre-registered M2. It failed both recorded runs (2.04x, 3.19x): each
        // new-plan sample pays the cleanup left by the preceding 10k receipt
        // scan, while back-to-back new-plan reads were flat. Landed on the
        // owner's override; see the 2026-10-19 br-kp1in.40 evidence file.
        assert!(
            large.0 <= 2 * small.0,
            "M2 newest-first grows with the inbox: {small:?} -> {large:?}"
        );
        assert!(g1 <= 1.35, "G1 sparse {g1:.3} > 1.35");
        assert!(g2 <= 1.35, "G2' rare urgent {g2:.3} > 1.35");
    }

    /// The receipt scan before this change, statement for statement: ordered
    /// pages, which the pinned engine serves by walking the agent's receipts
    /// from the start each time, and one header lookup per page.
    fn original_receipt_window(
        conn: &DbConn,
        filters: InboxFetchOptions<'_>,
    ) -> Result<NewestWindow, DbError> {
        let mut after = None;
        let mut newest = NewestWindow::new();
        loop {
            let (sql, params) = match after {
                None => (RECIPIENT_PAGE_ORDERED_SQL, vec![Value::BigInt(2)]),
                Some(id) => (
                    RECIPIENT_NEXT_ORDERED_SQL,
                    vec![Value::BigInt(2), Value::BigInt(id)],
                ),
            };
            let rows = query(conn, sql, &params)?;
            let finished = rows.len() < PAGE_SIZE;
            let mut page = HashMap::new();
            for row in rows {
                let id = row.get_named::<i64>("message_id").map_err(db_error)?;
                after = Some(id);
                let receipt = Receipt {
                    kind: row.get_named("kind").map_err(db_error)?,
                    read_ts: row.get_named("read_ts").map_err(db_error)?,
                    ack_ts: row.get_named("ack_ts").map_err(db_error)?,
                };
                if (filters.unread_only && receipt.read_ts.is_some())
                    || ((filters.ack_required_only || filters.ack_overdue_before.is_some())
                        && receipt.ack_ts.is_some())
                {
                    continue;
                }
                page.insert(id, receipt);
            }
            admit_headers(conn, 1, None, 20, filters, &mut page, &mut newest)?;
            if finished {
                return Ok(newest);
            }
        }
    }

    /// Median microseconds of the unordered and the ordered receipt scan, five
    /// alternating rounds, asserting identical rows every round.
    fn receipt_scans_us(conn: &DbConn, filters: InboxFetchOptions<'_>) -> (u128, u128) {
        let mut candidate = Vec::new();
        let mut reference = Vec::new();
        for round in 0..5 {
            let new = || {
                let start = Instant::now();
                let rows = with_snapshot(conn, || {
                    let window = receipt_window(conn, 1, 2, None, 20, filters)?;
                    hydrate(conn, 1, window, filters.body_policy)
                })
                .unwrap();
                (rows, start.elapsed().as_micros())
            };
            let old = || {
                let start = Instant::now();
                let rows = with_snapshot(conn, || {
                    let window = original_receipt_window(conn, filters)?;
                    hydrate(conn, 1, window, filters.body_policy)
                })
                .unwrap();
                (rows, start.elapsed().as_micros())
            };
            let (actual, expected) = if round % 2 == 0 {
                let actual = new();
                (actual, old())
            } else {
                let expected = old();
                (new(), expected)
            };
            assert_eq!(format!("{:?}", actual.0), format!("{:?}", expected.0));
            candidate.push(actual.1);
            reference.push(expected.1);
        }
        candidate.sort_unstable();
        reference.sort_unstable();
        (candidate[2], reference[2])
    }

    #[test]
    #[ignore = "explicit same-invocation unordered vs ordered receipt scan measurement (rscan_rule)"]
    fn receipt_scan_unordered_vs_ordered_pages() {
        use super::tests::{seed_walk, walk_conn};
        let filters = InboxFetchOptions {
            urgent_only: false,
            unread_only: true,
            ack_required_only: false,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        };
        let created: fn(i64) -> i64 = |id| 1_000_000 + id * 1000;
        // GreenStone receives 1 in 4 of the project's mail; 1 in 10 of hers is unread.
        let measure = |messages: i64| {
            let conn = walk_conn();
            seed_walk(&conn, (1, messages), 4, created, |_| "normal");
            conn.execute_raw(
                "UPDATE message_recipients SET read_ts = 1 WHERE agent_id = 2 AND message_id % 40 != 0; \
                 UPDATE message_recipients SET read_ts = NULL WHERE agent_id = 2 AND message_id % 40 = 0",
            )
            .unwrap();
            let (new, old) = receipt_scans_us(&conn, filters);
            let ratio = new as f64 / old.max(1) as f64;
            println!(
                "{{\"own_inbox\":{},\"unordered_median_us\":{new},\"ordered_median_us\":{old},\"ratio\":{ratio:.3}}}",
                messages / 4
            );
            (new, ratio)
        };
        let (_, two_page_ratio) = measure(1_200);
        let (small, _) = measure(4_000);
        let (large, large_ratio) = measure(40_000);
        assert!(
            large_ratio <= 0.25,
            "R1 own 10k unordered/ordered {large_ratio:.3} > 0.25"
        );
        assert!(
            large <= 15 * small,
            "R2 unordered scan grows faster than linear: {small} -> {large} us"
        );
        assert!(
            two_page_ratio <= 1.25,
            "R3 own 300 unordered/ordered {two_page_ratio:.3} > 1.25"
        );
    }
}
