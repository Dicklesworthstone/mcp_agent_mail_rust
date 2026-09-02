//! Synchronous database helpers.
//!
//! Exposes blocking DB queries used by UI loops and backgrounds threads
//! that cannot easily integrate with the async `sqlmodel_pool`.

use crate::DbConn;
use crate::error::DbError;
use crate::models::MessageRow;
use crate::queries::{InboxRow, UNKNOWN_SENDER_DISPLAY};
use sqlmodel_core::Value;

const MAX_SYNC_IN_CLAUSE_ITEMS: usize = 500;

/// A body-free, durable delivery event for one inbox recipient.
///
/// `seq` is a recipient-local cursor, not a message identifier. Consumers
/// must persist it only after processing the matching event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxDeliveryEvent {
    pub seq: i64,
    pub message_id: i64,
    pub kind: String,
    pub delivered_ts: i64,
    pub subject: String,
    pub sender_name: String,
    pub importance: String,
    pub ack_required: bool,
}

/// One oldest-first cursor page from [`inbox_delivery_events_from_conn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxDeliveryEventPage {
    pub events: Vec<InboxDeliveryEvent>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub oldest_available_cursor: Option<i64>,
    pub tail_cursor: i64,
}

/// One immutable observation that a message-specific recipient signal was
/// written to a delivery route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDeliverySignalReceipt {
    pub delivery_route: String,
    pub signal_path_digest: String,
    pub observed_ts: i64,
}

/// Per-recipient delivery facts for one persisted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDeliveryRecipientReceipt {
    pub recipient: String,
    pub kind: String,
    pub acknowledged_ts: Option<i64>,
    pub signal_receipts: Vec<MessageDeliverySignalReceipt>,
}

/// A message-ID-bound view of durable persistence, signal observations, and
/// acknowledgement state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDeliveryReceipt {
    pub message_id: i64,
    pub project_id: i64,
    pub persisted_ts: i64,
    pub recipients: Vec<MessageDeliveryRecipientReceipt>,
}

/// A cursor condition is explicit so monitor clients never confuse retention
/// loss or an invalid future position with a valid empty page.
#[derive(Debug, thiserror::Error)]
pub enum InboxDeliveryEventError {
    #[error("inbox delivery cursor {after} expired; oldest available cursor is {oldest_available}")]
    CursorExpired { after: i64, oldest_available: i64 },
    #[error("inbox delivery cursor {after} is ahead of durable tail {tail}")]
    CursorAhead { after: i64, tail: i64 },
    #[error("inbox delivery event query failed: {0}")]
    Database(#[from] DbError),
}

/// Append one successful signal observation for an exact message-recipient
/// pair.
///
/// `INSERT OR IGNORE` makes a retry idempotent without overwriting the
/// first durable observation or creating a second receipt.
pub fn append_message_delivery_signal_receipt(
    conn: &DbConn,
    message_id: i64,
    agent_id: i64,
    delivery_route: &str,
    signal_path_digest: &str,
    observed_ts: i64,
) -> Result<(), DbError> {
    conn.execute_sync(
        "INSERT OR IGNORE INTO message_delivery_signal_receipts \
             (message_id, agent_id, delivery_route, signal_path_digest, observed_ts) \
         SELECT ?, ?, ?, ?, ? \
         WHERE EXISTS (\
             SELECT 1 FROM message_recipients \
             WHERE message_id = ? AND agent_id = ?\
         )",
        &[
            Value::BigInt(message_id),
            Value::BigInt(agent_id),
            Value::Text(delivery_route.to_string()),
            Value::Text(signal_path_digest.to_string()),
            Value::BigInt(observed_ts),
            Value::BigInt(message_id),
            Value::BigInt(agent_id),
        ],
    )
    .map_err(|error| DbError::Sqlite(error.to_string()))?;

    let rows = conn
        .query_sync(
            "SELECT 1 FROM message_delivery_signal_receipts \
             WHERE message_id = ? AND agent_id = ? AND delivery_route = ? LIMIT 1",
            &[
                Value::BigInt(message_id),
                Value::BigInt(agent_id),
                Value::Text(delivery_route.to_string()),
            ],
        )
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    if rows.is_empty() {
        return Err(DbError::not_found(
            "MessageRecipient",
            format!("{message_id}:{agent_id}"),
        ));
    }

    Ok(())
}

/// Read persisted, signaled, and acknowledged facts for one exact message.
///
/// The mutable recipient `.signal` file is intentionally not read here: it is
/// a debounced latest-state hint that can point at another message. Only the
/// append-only receipt ledger establishes the `signaled` fact.
pub fn message_delivery_receipt_from_conn(
    conn: &DbConn,
    project_id: i64,
    message_id: i64,
) -> Result<MessageDeliveryReceipt, DbError> {
    let message_rows = conn
        .query_sync(
            "SELECT id, project_id, created_ts FROM messages WHERE id = ? AND project_id = ? LIMIT 1",
            &[Value::BigInt(message_id), Value::BigInt(project_id)],
        )
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let message = message_rows
        .first()
        .ok_or_else(|| DbError::not_found("Message", message_id.to_string()))?;
    let persisted_ts = message
        .get_named("created_ts")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;

    let rows = conn
        .query_sync(
            "SELECT agent.name AS recipient, recipient.kind AS kind, recipient.ack_ts AS ack_ts, \
                    signal.delivery_route AS delivery_route, \
                    signal.signal_path_digest AS signal_path_digest, \
                    signal.observed_ts AS observed_ts \
             FROM message_recipients AS recipient \
             JOIN agents AS agent ON agent.id = recipient.agent_id \
             LEFT JOIN message_delivery_signal_receipts AS signal \
               ON signal.message_id = recipient.message_id AND signal.agent_id = recipient.agent_id \
             WHERE recipient.message_id = ? \
             ORDER BY recipient.agent_id ASC, signal.delivery_route ASC",
            &[Value::BigInt(message_id)],
        )
        .map_err(|error| DbError::Sqlite(error.to_string()))?;

    let mut recipients: Vec<MessageDeliveryRecipientReceipt> = Vec::new();
    for row in rows {
        let recipient: String = row
            .get_named("recipient")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let kind: String = row
            .get_named("kind")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        if recipients
            .last()
            .is_none_or(|previous| previous.recipient != recipient || previous.kind != kind)
        {
            let acknowledged_ts = row
                .get_named("ack_ts")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            recipients.push(MessageDeliveryRecipientReceipt {
                recipient: recipient.clone(),
                kind: kind.clone(),
                acknowledged_ts,
                signal_receipts: Vec::new(),
            });
        }

        let delivery_route: Option<String> = row
            .get_named("delivery_route")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let signal_path_digest: Option<String> = row
            .get_named("signal_path_digest")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let observed_ts: Option<i64> = row
            .get_named("observed_ts")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        if let (Some(delivery_route), Some(signal_path_digest), Some(observed_ts)) =
            (delivery_route, signal_path_digest, observed_ts)
            && let Some(current) = recipients.last_mut()
        {
            current.signal_receipts.push(MessageDeliverySignalReceipt {
                delivery_route,
                signal_path_digest,
                observed_ts,
            });
        }
    }

    Ok(MessageDeliveryReceipt {
        message_id,
        project_id,
        persisted_ts,
        recipients,
    })
}

/// Synchronously update the thread ID of a message.
///
/// Returns `Ok(true)` if the thread ID was updated, `Ok(false)` if it was already the target ID.
/// Returns `Err` if the message was not found or if a database error occurred.
pub fn update_message_thread_id(
    conn: &DbConn,
    message_id: i64,
    target_thread_id: &str,
) -> Result<bool, DbError> {
    let target_thread_id = target_thread_id.trim();
    if target_thread_id.is_empty() {
        return Ok(false);
    }

    let lookup_sql = "SELECT thread_id FROM messages WHERE id = ? LIMIT 1";
    let rows = conn
        .query_sync(lookup_sql, &[Value::BigInt(message_id)])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let mut row_iter = rows.into_iter();
    let row = row_iter.next().ok_or_else(|| DbError::NotFound {
        entity: "Message",
        identifier: message_id.to_string(),
    })?;

    let current_thread_id = row.get_named::<String>("thread_id").ok();

    if current_thread_id.as_deref() == Some(target_thread_id) {
        return Ok(false);
    }

    let update_sql = "UPDATE messages SET thread_id = ? WHERE id = ?";
    conn.execute_sync(
        update_sql,
        &[
            Value::Text(target_thread_id.to_string()),
            Value::BigInt(message_id),
        ],
    )
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    Ok(true)
}

pub fn fetch_inbox_rows_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_inbox_rows_from_conn_impl(
        conn,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxFetchOptions {
            urgent_only,
            unread_only,
            ack_required_only,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::Full,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn fetch_inbox_metadata_rows_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_inbox_rows_from_conn_impl(
        conn,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxFetchOptions {
            urgent_only,
            unread_only,
            ack_required_only,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn fetch_inbox_ack_overdue_rows_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    ack_overdue_before: i64,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_inbox_rows_from_conn_impl(
        conn,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxFetchOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: Some(ack_overdue_before),
            topic: None,
            body_policy: InboxBodyPolicy::Full,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn fetch_inbox_ack_overdue_metadata_rows_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    ack_overdue_before: i64,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_inbox_rows_from_conn_impl(
        conn,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxFetchOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: Some(ack_overdue_before),
            topic: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
}

/// Whether inbox rows materialize `body_md` or metadata only.
#[derive(Clone, Copy)]
pub enum InboxBodyPolicy {
    Full,
    MetadataOnly,
}

/// Filter set for [`fetch_inbox_filtered_rows_from_conn`].
#[derive(Clone, Copy)]
pub struct InboxFetchOptions<'a> {
    pub urgent_only: bool,
    pub unread_only: bool,
    pub ack_required_only: bool,
    pub ack_overdue_before: Option<i64>,
    pub topic: Option<&'a str>,
    pub body_policy: InboxBodyPolicy,
}

/// Fetch inbox rows with the complete filter set used by the MCP tool.
pub fn fetch_inbox_filtered_rows_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    fetch_inbox_rows_from_conn_impl(conn, project_id, agent_id, since_ts, limit, options)
}

fn fetch_inbox_rows_from_conn_impl(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    let _ = conn.execute_raw("PRAGMA busy_timeout = 250");
    let body_select = match options.body_policy {
        InboxBodyPolicy::Full => "m.body_md",
        InboxBodyPolicy::MetadataOnly => "'' AS body_md",
    };

    let mut sql = format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.topic, m.subject, {body_select}, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                r.kind, COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, r.read_ts, r.ack_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         LEFT JOIN agents s ON s.id = m.sender_id \
         WHERE r.agent_id = ? AND m.project_id = ?"
    );

    let mut params = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];
    if options.urgent_only {
        sql.push_str(" AND m.importance IN ('high', 'urgent')");
    }
    if options.unread_only {
        sql.push_str(" AND r.read_ts IS NULL");
    }
    if options.ack_required_only {
        sql.push_str(" AND m.ack_required = 1 AND r.ack_ts IS NULL");
    }
    if let Some(threshold) = options.ack_overdue_before {
        sql.push_str(" AND m.ack_required = 1 AND r.ack_ts IS NULL AND m.created_ts < ?");
        params.push(Value::BigInt(threshold));
    }
    if let Some(ts) = since_ts {
        sql.push_str(" AND m.created_ts > ?");
        params.push(Value::BigInt(ts));
    }
    if let Some(topic) = options.topic {
        sql.push_str(" AND m.topic = ? COLLATE NOCASE");
        params.push(Value::Text(topic.to_string()));
    }

    let limit_i64 =
        i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    sql.push_str(" ORDER BY m.created_ts DESC LIMIT ?");
    params.push(Value::BigInt(limit_i64));

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row
            .get_named("id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let project_id: i64 = row
            .get_named("project_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let sender_id: i64 = row
            .get_named("sender_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let thread_id: Option<String> = row
            .get_named("thread_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let topic: Option<String> = row
            .get_named("topic")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let subject: String = row
            .get_named("subject")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let body_md: String = row
            .get_named("body_md")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let importance: String = row
            .get_named("importance")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let ack_required: i64 = row
            .get_named("ack_required")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let created_ts: i64 = row
            .get_named("created_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let recipients_json: String = row
            .get_named("recipients_json")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let attachments: String = row
            .get_named("attachments")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let kind: String = row
            .get_named("kind")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let sender_name: String = row
            .get_named("sender_name")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let read_ts: Option<i64> = row
            .get_named("read_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let ack_ts: Option<i64> = row
            .get_named("ack_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;

        out.push(InboxRow {
            message: MessageRow {
                id: Some(id),
                project_id,
                sender_id,
                thread_id,
                topic,
                subject,
                body_md,
                importance,
                ack_required,
                created_ts,
                recipients_json,
                attachments,
            },
            kind,
            sender_name,
            read_ts,
            ack_ts,
        });
    }

    Ok(out)
}

/// Read an append-only, recipient-scoped inbox delivery page.
///
/// This intentionally does not inspect `read_ts`: marking mail read changes a
/// snapshot but never changes which delivery events a monitor must observe.
/// Rows are ordered oldest-first so a consumer can safely persist
/// `next_cursor` after processing each page.
pub fn inbox_delivery_events_from_conn(
    conn: &DbConn,
    project_id: i64,
    agent_id: i64,
    after: Option<i64>,
    limit: usize,
) -> Result<InboxDeliveryEventPage, InboxDeliveryEventError> {
    if limit == 0 {
        return Err(InboxDeliveryEventError::Database(DbError::invalid(
            "limit",
            "inbox event limit must be at least 1",
        )));
    }
    let limit_with_probe = limit.checked_add(1).ok_or_else(|| {
        InboxDeliveryEventError::Database(DbError::invalid(
            "limit",
            "inbox event limit is too large",
        ))
    })?;
    let limit_i64 = i64::try_from(limit_with_probe).map_err(|_| {
        InboxDeliveryEventError::Database(DbError::invalid(
            "limit",
            "inbox event limit exceeds i64::MAX",
        ))
    })?;

    let range_rows = conn
        .query_sync(
            "SELECT MIN(seq) AS oldest_cursor, MAX(seq) AS tail_cursor, \
                    (SELECT MIN(seq) FROM inbox_delivery_events) AS global_oldest \
             FROM inbox_delivery_events WHERE project_id = ? AND agent_id = ?",
            &[Value::BigInt(project_id), Value::BigInt(agent_id)],
        )
        .map_err(|error| InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string())))?;
    let (oldest_available_cursor, tail_cursor, global_oldest_cursor) = range_rows
        .first()
        .map(
            |row| -> Result<(Option<i64>, i64, Option<i64>), InboxDeliveryEventError> {
                let oldest = row
                    .get_named::<Option<i64>>("oldest_cursor")
                    .map_err(|error| {
                        InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
                    })?;
                let tail = row
                    .get_named::<Option<i64>>("tail_cursor")
                    .map_err(|error| {
                        InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
                    })?
                    .unwrap_or(0);
                let global_oldest =
                    row.get_named::<Option<i64>>("global_oldest")
                        .map_err(|error| {
                            InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
                        })?;
                Ok((oldest, tail, global_oldest))
            },
        )
        .transpose()?
        .unwrap_or((None, 0, None));

    if let Some(after) = after {
        // `seq` is a GLOBAL AUTOINCREMENT shared by every recipient, so a gap
        // between `after` and this recipient's oldest event normally consists
        // of other recipients' deliveries — NOT lost history. A monitor that
        // called `--position-now` on an empty inbox (cursor 0) must still
        // receive its first delivery even when that lands at a high global
        // seq (GH#238). A cursor is only genuinely expired when retention has
        // actually removed rows, which is observable while global seq 1 is
        // gone from the ledger.
        let retention_has_pruned = global_oldest_cursor.is_some_and(|global| global > 1);
        if let Some(oldest) = oldest_available_cursor
            && retention_has_pruned
            && after < oldest.saturating_sub(1)
        {
            return Err(InboxDeliveryEventError::CursorExpired {
                after,
                oldest_available: oldest,
            });
        }
        if after > tail_cursor {
            return Err(InboxDeliveryEventError::CursorAhead {
                after,
                tail: tail_cursor,
            });
        }
    }

    let cursor = after.unwrap_or(0);
    let rows = conn
        .query_sync(
            "SELECT e.seq, e.message_id, e.kind, e.delivered_ts, m.subject, \
                    COALESCE(sender.name, ?) AS sender_name, m.importance, m.ack_required \
             FROM inbox_delivery_events AS e \
             JOIN messages AS m ON m.id = e.message_id \
             LEFT JOIN agents AS sender ON sender.id = m.sender_id \
             WHERE e.project_id = ? AND e.agent_id = ? AND e.seq > ? \
             ORDER BY e.seq ASC LIMIT ?",
            &[
                Value::Text(UNKNOWN_SENDER_DISPLAY.to_string()),
                Value::BigInt(project_id),
                Value::BigInt(agent_id),
                Value::BigInt(cursor),
                Value::BigInt(limit_i64),
            ],
        )
        .map_err(|error| InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string())))?;

    let mut events = Vec::with_capacity(rows.len().min(limit));
    for row in rows {
        events.push(InboxDeliveryEvent {
            seq: row.get_named("seq").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            message_id: row.get_named("message_id").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            kind: row.get_named("kind").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            delivered_ts: row.get_named("delivered_ts").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            subject: row.get_named("subject").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            sender_name: row.get_named("sender_name").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            importance: row.get_named("importance").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })?,
            ack_required: row.get_named::<i64>("ack_required").map_err(|error| {
                InboxDeliveryEventError::Database(DbError::Sqlite(error.to_string()))
            })? != 0,
        });
    }
    let has_more = events.len() > limit;
    if has_more {
        events.pop();
    }
    let next_cursor = events.last().map_or(cursor, |event| event.seq);

    Ok(InboxDeliveryEventPage {
        events,
        next_cursor,
        has_more,
        oldest_available_cursor,
        tail_cursor,
    })
}

/// Fetch inbox rows using a short-lived synchronous FrankenSQLite connection.
#[allow(clippy::too_many_arguments)]
pub fn fetch_inbox_sync_by_ids(
    sqlite_path: &str,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<InboxRow>, DbError> {
    let conn = if sqlite_path == ":memory:" {
        DbConn::open_memory()
    } else {
        DbConn::open_file(sqlite_path)
    }
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let result = fetch_inbox_rows_from_conn(
        &conn,
        project_id,
        agent_id,
        urgent_only,
        unread_only,
        ack_required_only,
        since_ts,
        limit,
    );

    crate::close_db_conn(conn, "fetch_inbox_sync_by_ids connection");
    result
}

fn open_sync_conn(sqlite_path: &str) -> Result<DbConn, DbError> {
    let conn = if sqlite_path == ":memory:" {
        DbConn::open_memory()
    } else {
        DbConn::open_file(sqlite_path.to_string())
    }
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    // br-ovy6e: keep in lockstep with the runtime pragma bundles. These
    // blocking helpers serve UI loops and dispatch-adjacent sync paths, so
    // they must not out-sleep the 30s ecosystem client deadline.
    let _ = conn.execute_raw(&format!(
        "PRAGMA busy_timeout = {}",
        mcp_agent_mail_core::config::DB_RUNTIME_BUSY_TIMEOUT_MS
    ));
    Ok(conn)
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

fn is_missing_inbox_stats_table_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("no such table") && lowered.contains("inbox_stats")
}

#[derive(Debug, Clone, Copy)]
struct SyncAgentInboxStatsRebuild {
    total_count: i64,
    unread_count: i64,
    ack_pending_count: i64,
    last_message_ts: Option<i64>,
}

fn compute_agent_inbox_stats_sync(
    conn: &DbConn,
    agent_id: i64,
) -> Result<Option<SyncAgentInboxStatsRebuild>, DbError> {
    let sql = "\
        SELECT \
            COUNT(*) AS total_count, \
            SUM(CASE WHEN read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
            SUM(CASE \
                WHEN ack_ts IS NULL \
                 AND message_id IN (SELECT id FROM messages WHERE ack_required = 1) \
                THEN 1 ELSE 0 END) AS ack_pending_count, \
            (SELECT MAX(created_ts) \
               FROM messages \
              WHERE id IN (SELECT message_id \
                             FROM message_recipients \
                            WHERE agent_id = ?)) AS last_message_ts \
        FROM message_recipients \
        WHERE agent_id = ? \
          AND message_id IN (SELECT id FROM messages)";
    let rows = conn
        .query_sync(sql, &[Value::BigInt(agent_id), Value::BigInt(agent_id)])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;
    let row = rows.first().ok_or_else(|| {
        DbError::Internal(format!(
            "inbox_stats rebuild returned no aggregate row for agent_id={agent_id}"
        ))
    })?;
    let total_count = row.get_named::<i64>("total_count").unwrap_or(0);
    if total_count == 0 {
        return Ok(None);
    }
    Ok(Some(SyncAgentInboxStatsRebuild {
        total_count,
        unread_count: row.get_named::<i64>("unread_count").unwrap_or(0),
        ack_pending_count: row.get_named::<i64>("ack_pending_count").unwrap_or(0),
        last_message_ts: row.get_named::<i64>("last_message_ts").ok(),
    }))
}

fn insert_agent_inbox_stats_sync(
    conn: &DbConn,
    agent_id: i64,
    stats: SyncAgentInboxStatsRebuild,
) -> Result<(), DbError> {
    let sql = "INSERT INTO inbox_stats \
         (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
         VALUES (?, ?, ?, ?, ?)";
    let last_message_ts = stats.last_message_ts.map_or(Value::Null, Value::BigInt);
    conn.execute_sync(
        sql,
        &[
            Value::BigInt(agent_id),
            Value::BigInt(stats.total_count),
            Value::BigInt(stats.unread_count),
            Value::BigInt(stats.ack_pending_count),
            last_message_ts,
        ],
    )
    .map(|_| ())
    .map_err(|e| DbError::Sqlite(e.to_string()))
}

fn rebuild_agent_inbox_stats_sync(conn: &DbConn, agent_id: i64) -> Result<(), DbError> {
    let params = [Value::BigInt(agent_id)];
    match conn.execute_sync("DELETE FROM inbox_stats WHERE agent_id = ?", &params) {
        Ok(_) => {}
        Err(err) => {
            let message = err.to_string();
            if is_missing_inbox_stats_table_error(&message) {
                return Ok(());
            }
            return Err(DbError::Sqlite(message));
        }
    }

    let Some(stats) = compute_agent_inbox_stats_sync(conn, agent_id)? else {
        return Ok(());
    };
    match insert_agent_inbox_stats_sync(conn, agent_id, stats) {
        Ok(()) => Ok(()),
        Err(err) => {
            let message = err.to_string();
            if is_missing_inbox_stats_table_error(&message) {
                Ok(())
            } else {
                Err(DbError::Sqlite(message))
            }
        }
    }
}

/// Messages changed by a synchronous batch mark-read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMessagesReadBatch {
    /// Timestamp written to each updated recipient row.
    pub read_ts: i64,
    /// Message IDs whose recipient rows were previously unread and were updated.
    pub message_ids: Vec<i64>,
}

fn mark_messages_read_batch_sync_conn(
    conn: &DbConn,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<Option<MarkMessagesReadBatch>, DbError> {
    if message_ids.is_empty() {
        return Ok(None);
    }

    let mut unique_message_ids = message_ids.to_vec();
    unique_message_ids.sort_unstable();
    unique_message_ids.dedup();
    let read_ts = crate::now_micros();

    begin_sync_write_tx(conn)?;

    let result = (|| -> Result<Vec<i64>, DbError> {
        let mut updated_message_ids = Vec::new();
        for chunk in unique_message_ids.chunks(MAX_SYNC_IN_CLAUSE_ITEMS) {
            let select_sql = format!(
                "SELECT DISTINCT message_id FROM message_recipients \
                 WHERE agent_id = ? AND read_ts IS NULL AND message_id IN ({})",
                placeholders(chunk.len())
            );
            let mut select_params = Vec::with_capacity(1 + chunk.len());
            select_params.push(Value::BigInt(agent_id));
            for &message_id in chunk {
                select_params.push(Value::BigInt(message_id));
            }
            for row in conn
                .query_sync(&select_sql, &select_params)
                .map_err(|e| DbError::Sqlite(e.to_string()))?
            {
                let message_id = row
                    .get_named::<i64>("message_id")
                    .map_err(|e| DbError::Sqlite(e.to_string()))?;
                updated_message_ids.push(message_id);
            }

            let sql = format!(
                "UPDATE message_recipients \
                 SET read_ts = ? \
                 WHERE agent_id = ? AND read_ts IS NULL AND message_id IN ({})",
                placeholders(chunk.len())
            );
            let mut params = Vec::with_capacity(2 + chunk.len());
            params.push(Value::BigInt(read_ts));
            params.push(Value::BigInt(agent_id));
            for &message_id in chunk {
                params.push(Value::BigInt(message_id));
            }
            conn.execute_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
        }

        rebuild_agent_inbox_stats_sync(conn, agent_id)?;
        updated_message_ids.sort_unstable();
        updated_message_ids.dedup();
        Ok(updated_message_ids)
    })();

    match result {
        Ok(updated_message_ids) => {
            commit_sync_write_tx(conn)?;
            Ok(
                (!updated_message_ids.is_empty()).then_some(MarkMessagesReadBatch {
                    read_ts,
                    message_ids: updated_message_ids,
                }),
            )
        }
        Err(err) => {
            rollback_sync_write_tx(conn);
            Err(err)
        }
    }
}

/// Synchronously batch-mark multiple messages as read for a single agent.
///
/// This bypasses the async MVCC write path used by `fetch_inbox` follow-up
/// auto-read handling. It is intended for operational mailbox reads where a
/// direct SQLite transaction is more reliable than the pooled async path.
pub fn mark_messages_read_batch_sync(
    sqlite_path: &str,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<Option<MarkMessagesReadBatch>, DbError> {
    if message_ids.is_empty() {
        return Ok(None);
    }

    let conn = open_sync_conn(sqlite_path)?;
    let result = mark_messages_read_batch_sync_conn(&conn, agent_id, message_ids);
    crate::close_db_conn(conn, "mark_messages_read_batch_sync connection");
    result
}

fn begin_sync_write_tx(conn: &DbConn) -> Result<(), DbError> {
    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map(|_| ())
        .map_err(|e| DbError::Sqlite(e.to_string()))
}

fn commit_sync_write_tx(conn: &DbConn) -> Result<(), DbError> {
    conn.execute_sync("COMMIT", &[])
        .map(|_| ())
        .map_err(|e| DbError::Sqlite(e.to_string()))
}

fn rollback_sync_write_tx(conn: &DbConn) {
    let _ = conn.execute_sync("ROLLBACK", &[]);
}

fn is_agent_name_unique_violation(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("unique constraint failed")
        && normalized.contains("agents.project_id")
        && normalized.contains("agents.name")
}

fn lookup_agent_id_by_name(
    conn: &DbConn,
    project_id: i64,
    agent_name: &str,
) -> Result<Option<i64>, DbError> {
    let rows = conn
        .query_sync(
            "SELECT id FROM agents \
             WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE \
             ORDER BY id ASC LIMIT 1",
            &[
                Value::BigInt(project_id),
                Value::Text(agent_name.trim().to_string()),
            ],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.get_named::<i64>("id").ok()))
}

fn resolve_root_project_id(conn: &DbConn) -> Result<i64, DbError> {
    let project_row = conn
        .query_sync("SELECT id FROM projects ORDER BY id LIMIT 1", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?
        .into_iter()
        .next();

    project_row
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::NotFound {
            entity: "Project",
            identifier: "any".into(),
        })
}

fn resolve_or_create_sender_id(
    conn: &DbConn,
    project_id: i64,
    sender_name: &str,
    now: i64,
) -> Result<i64, DbError> {
    if let Some(sender_id) = lookup_agent_id_by_name(conn, project_id, sender_name)? {
        return Ok(sender_id);
    }

    match conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) \
         VALUES (?1, ?2, 'tui-overseer', 'human', 'Human operator via TUI', ?3, ?4)",
        &[
            Value::BigInt(project_id),
            Value::Text(sender_name.trim().to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
        ],
    ) {
        Ok(_) => {}
        Err(err) => {
            let message = err.to_string();
            if is_agent_name_unique_violation(&message)
                && let Some(sender_id) = lookup_agent_id_by_name(conn, project_id, sender_name)?
            {
                return Ok(sender_id);
            }
            return Err(DbError::Sqlite(message));
        }
    }

    lookup_agent_id_by_name(conn, project_id, sender_name)?
        .ok_or_else(|| DbError::Internal("Failed to resolve sender ID after insert".into()))
}

struct RootMessageInput<'a> {
    subject: &'a str,
    body_md: &'a str,
    importance: &'a str,
    thread_id: Option<&'a str>,
}

fn insert_root_message(
    conn: &DbConn,
    project_id: i64,
    sender_id: i64,
    now: i64,
    message: &RootMessageInput<'_>,
) -> Result<i64, DbError> {
    let thread_id_val = message
        .thread_id
        .map(str::trim)
        .filter(|tid| !tid.is_empty())
        .map_or(Value::Null, |tid| Value::Text(tid.to_string()));

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(sender_id),
            Value::Text(message.subject.to_string()),
            Value::Text(message.body_md.to_string()),
            Value::Text(message.importance.to_string()),
            thread_id_val,
            Value::BigInt(now),
        ],
    )
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let msg_rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;
    msg_rows
        .into_iter()
        .next()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::Internal("Message insert returned no ID".into()))
}

fn insert_message_recipients(
    conn: &DbConn,
    project_id: i64,
    msg_id: i64,
    recipients: &[(String, String)],
) -> Result<(), DbError> {
    use std::collections::HashSet;

    let mut inserted_recipient_ids: HashSet<i64> = HashSet::new();
    let mut missing_names: Vec<String> = Vec::new();
    let mut missing_seen: HashSet<String> = HashSet::new();

    for (name, kind) in recipients {
        let Some(aid) = lookup_agent_id_by_name(conn, project_id, name)? else {
            let normalized = name.trim().to_ascii_lowercase();
            if missing_seen.insert(normalized) {
                missing_names.push(name.trim().to_string());
            }
            continue;
        };

        if inserted_recipient_ids.insert(aid) {
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, ?3)",
                &[
                    Value::BigInt(msg_id),
                    Value::BigInt(aid),
                    Value::Text(kind.clone()),
                ],
            )
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        }
    }

    if !missing_names.is_empty() {
        return Err(DbError::not_found(
            "Agent",
            format!(
                "unknown recipients in project {project_id}: {}",
                missing_names.join(", ")
            ),
        ));
    }

    Ok(())
}

fn sync_message_recipients_json(conn: &DbConn, msg_id: i64) -> Result<(), DbError> {
    let rows = conn
        .query_sync(
            "SELECT COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') AS name, \
                    mr.kind AS kind \
             FROM message_recipients mr \
             LEFT JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                     COALESCE(NULLIF(TRIM(a.name), ''), '[unknown-agent-' || mr.agent_id || ']') COLLATE NOCASE",
            &[Value::BigInt(msg_id)],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let recipients_json = rows
        .into_iter()
        .map(|row| {
            let name = row
                .get_named::<String>("name")
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
            let kind = row
                .get_named::<String>("kind")
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
            Ok(serde_json::json!({
                "name": name,
                "kind": kind,
            }))
        })
        .collect::<Result<Vec<_>, DbError>>()
        .and_then(|payload| {
            serde_json::to_string(&payload)
                .map_err(|e| DbError::Internal(format!("failed to encode recipients JSON: {e}")))
        })?;

    conn.execute_sync(
        "UPDATE messages SET recipients_json = ? WHERE id = ?",
        &[Value::Text(recipients_json), Value::BigInt(msg_id)],
    )
    .map(|_| ())
    .map_err(|e| DbError::Sqlite(e.to_string()))
}

/// Dispatch a message from the first available project (TUI context).
///
/// Handles project resolution, sender auto-registration (for overseer),
/// message insertion, and recipient linking in a single transaction.
pub fn dispatch_root_message(
    conn: &DbConn,
    sender_name: &str,
    subject: &str,
    body_md: &str,
    importance: &str,
    thread_id: Option<&str>,
    recipients: &[(String, String)], // (name, kind)
) -> Result<i64, DbError> {
    use crate::timestamps::now_micros;

    let project_id = resolve_root_project_id(conn)?;
    begin_sync_write_tx(conn)?;

    let dispatch_result = (|| -> Result<i64, DbError> {
        let now = now_micros();
        let sender_id = resolve_or_create_sender_id(conn, project_id, sender_name, now)?;
        let message_input = RootMessageInput {
            subject,
            body_md,
            importance,
            thread_id,
        };
        let msg_id = insert_root_message(conn, project_id, sender_id, now, &message_input)?;
        insert_message_recipients(conn, project_id, msg_id, recipients)?;
        sync_message_recipients_json(conn, msg_id)?;
        Ok(msg_id)
    })();

    match dispatch_result {
        Ok(msg_id) => {
            commit_sync_write_tx(conn)?;
            // GH#227: every delivery path must invalidate the process-wide
            // search cache once the message row is committed. This sync path
            // is the TUI compose dispatch, which runs inside the server
            // process — without this, cached pre-delivery result sets keep
            // serving false-negatives for the full cache TTL.
            crate::search_service::invalidate_search_cache(
                crate::search_cache::InvalidationTrigger::IndexUpdate,
            );
            Ok(msg_id)
        }
        Err(err) => {
            rollback_sync_write_tx(conn);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn block_on<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(asupersync::Cx) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cx = asupersync::Cx::for_testing();
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    }

    /// Helper: open an in-memory DB with the full schema applied.
    fn test_conn() -> DbConn {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
            .expect("apply PRAGMAs");
        block_on({
            let conn = &conn;
            move |cx| async move {
                schema::migrate_to_latest_base(&cx, conn)
                    .await
                    .into_result()
                    .expect("init schema migrations");
            }
        });
        conn
    }

    /// Insert a project and return its id.
    fn insert_project(conn: &DbConn) -> i64 {
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp/test', 1000000)",
            &[],
        )
        .expect("insert project");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get project id")
    }

    /// Insert an agent and return its id.
    fn insert_agent(conn: &DbConn, project_id: i64, name: &str) -> i64 {
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (?1, ?2, 'test', 'test', 'test', 1000000, 1000000)",
            &[Value::BigInt(project_id), Value::Text(name.to_string())],
        )
        .expect("insert agent");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get agent id")
    }

    /// Insert a message and return its id.
    fn insert_message(conn: &DbConn, project_id: i64, sender_id: i64, thread_id: &str) -> i64 {
        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts) \
             VALUES (?1, ?2, 'test subject', 'test body', 'normal', 0, ?3, 1000000)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                Value::Text(thread_id.to_string()),
            ],
        )
        .expect("insert message");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get message id")
    }

    #[test]
    fn fetch_inbox_metadata_rows_omit_body_payload() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient");
        let msg_id = insert_message(&conn, pid, sender_id, "thread-1");
        conn.execute_sync(
            "UPDATE messages SET body_md = ? WHERE id = ?",
            &[
                Value::Text("large body payload that metadata reads should not load".repeat(8)),
                Value::BigInt(msg_id),
            ],
        )
        .expect("update message body");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(msg_id), Value::BigInt(recipient_id)],
        )
        .expect("insert recipient");

        let full_rows =
            fetch_inbox_rows_from_conn(&conn, pid, recipient_id, false, false, false, None, 10)
                .expect("full inbox fetch");
        assert_eq!(full_rows.len(), 1);
        assert!(
            full_rows[0]
                .message
                .body_md
                .starts_with("large body payload")
        );

        let metadata_rows = fetch_inbox_metadata_rows_from_conn(
            &conn,
            pid,
            recipient_id,
            false,
            false,
            false,
            None,
            10,
        )
        .expect("metadata inbox fetch");
        assert_eq!(metadata_rows.len(), 1);
        assert_eq!(metadata_rows[0].message.id, Some(msg_id));
        assert_eq!(metadata_rows[0].message.subject, "test subject");
        assert!(
            metadata_rows[0].message.body_md.is_empty(),
            "metadata-only inbox reads should not materialize message bodies"
        );
    }

    #[test]
    fn inbox_delivery_events_are_append_only_paginated_and_cursor_checked() {
        let conn = test_conn();
        let project_id = insert_project(&conn);
        let sender_id = insert_agent(&conn, project_id, "Sender");
        let recipient_id = insert_agent(&conn, project_id, "Recipient");
        let mut message_ids = Vec::new();
        for thread_id in ["thread-1", "thread-2", "thread-3"] {
            let message_id = insert_message(&conn, project_id, sender_id, thread_id);
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
                &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
            )
            .expect("insert recipient and durable delivery event");
            message_ids.push(message_id);
        }

        let first = inbox_delivery_events_from_conn(&conn, project_id, recipient_id, None, 2)
            .expect("first event page");
        assert_eq!(first.events.len(), 2);
        assert!(first.has_more);
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.message_id)
                .collect::<Vec<_>>(),
            message_ids[..2]
        );
        assert!(
            first.events[0].seq < first.events[1].seq,
            "recipient events must be oldest-first by independent cursor"
        );
        assert_eq!(first.events[0].subject, "test subject");
        assert_eq!(first.events[0].sender_name, "Sender");
        assert_eq!(first.events[0].kind, "to");

        // Draining unread state changes the snapshot but never rewrites or
        // hides a delivery event that a restart-safe monitor still needs.
        conn.execute_sync(
            "UPDATE message_recipients SET read_ts = 2000000 WHERE agent_id = ?1",
            &[Value::BigInt(recipient_id)],
        )
        .expect("mark recipients read");
        let second = inbox_delivery_events_from_conn(
            &conn,
            project_id,
            recipient_id,
            Some(first.next_cursor),
            2,
        )
        .expect("second event page");
        assert_eq!(second.events.len(), 1);
        assert!(!second.has_more);
        assert_eq!(second.events[0].message_id, message_ids[2]);
        assert_eq!(second.next_cursor, second.events[0].seq);

        let ahead = inbox_delivery_events_from_conn(
            &conn,
            project_id,
            recipient_id,
            Some(second.tail_cursor + 1),
            1,
        )
        .expect_err("future cursors must never look like empty delivery");
        assert!(matches!(ahead, InboxDeliveryEventError::CursorAhead { .. }));

        conn.execute_sync(
            "DELETE FROM inbox_delivery_events WHERE seq = ?1",
            &[Value::BigInt(first.events[0].seq)],
        )
        .expect("simulate a pruned historical event");
        let expired = inbox_delivery_events_from_conn(&conn, project_id, recipient_id, Some(0), 1)
            .expect_err("cursor before retained floor must be explicit");
        assert!(matches!(
            expired,
            InboxDeliveryEventError::CursorExpired { .. }
        ));
    }

    #[test]
    fn bootstrap_cursor_survives_unrelated_recipients_advancing_global_seq() {
        // GH#238: `seq` is a global AUTOINCREMENT. A monitor that positioned
        // itself on an empty inbox (cursor 0) must receive its first delivery
        // even when unrelated recipients have advanced the global sequence in
        // the meantime — the gap below its own oldest event is other
        // recipients' traffic, not lost history, and must never read as
        // CURSOR_EXPIRED while nothing has been pruned.
        let conn = test_conn();
        let project_id = insert_project(&conn);
        let sender_id = insert_agent(&conn, project_id, "Sender");
        let other_id = insert_agent(&conn, project_id, "Other");
        let late_id = insert_agent(&conn, project_id, "Latecomer");

        // Position-now on an empty inbox: tail 0, no events.
        let empty = inbox_delivery_events_from_conn(&conn, project_id, late_id, None, 5)
            .expect("empty inbox page");
        assert_eq!(empty.events, [] as [InboxDeliveryEvent; 0]);
        assert_eq!(empty.tail_cursor, 0);

        // Unrelated traffic advances the global sequence well past 0.
        for _ in 0..5 {
            let noise_id = insert_message(&conn, project_id, sender_id, "noise-thread");
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
                &[Value::BigInt(noise_id), Value::BigInt(other_id)],
            )
            .expect("persist unrelated recipient");
        }

        // The latecomer's FIRST delivery lands at a high global seq.
        let handoff_id = insert_message(&conn, project_id, sender_id, "handoff-thread");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(handoff_id), Value::BigInt(late_id)],
        )
        .expect("persist latecomer recipient");

        // Resuming from the persisted cursor 0 must deliver it.
        let page = inbox_delivery_events_from_conn(&conn, project_id, late_id, Some(0), 5)
            .expect("bootstrap cursor must not expire while nothing was pruned (GH#238)");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].message_id, handoff_id);
        assert!(page.oldest_available_cursor.expect("own oldest") > 1);
    }

    #[test]
    fn message_delivery_signal_receipts_are_message_bound_and_ack_separate() {
        let conn = test_conn();
        let project_id = insert_project(&conn);
        let sender_id = insert_agent(&conn, project_id, "Sender");
        let recipient_id = insert_agent(&conn, project_id, "Recipient");
        let first_message_id = insert_message(&conn, project_id, sender_id, "first-thread");
        let second_message_id = insert_message(&conn, project_id, sender_id, "second-thread");
        for message_id in [first_message_id, second_message_id] {
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
                &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
            )
            .expect("persist recipient");
        }

        let persisted_only =
            message_delivery_receipt_from_conn(&conn, project_id, first_message_id)
                .expect("read persisted receipt");
        assert_eq!(persisted_only.recipients.len(), 1);
        assert_eq!(
            persisted_only.recipients[0].signal_receipts,
            [] as [MessageDeliverySignalReceipt; 0]
        );
        assert_eq!(persisted_only.recipients[0].acknowledged_ts, None);

        append_message_delivery_signal_receipt(
            &conn,
            first_message_id,
            recipient_id,
            "filesystem_signal",
            "first-path-digest",
            1_700_000_000_000_000,
        )
        .expect("append first message signal receipt");
        // A retry must not overwrite the first observation or append a second
        // receipt for the same (message, recipient, route).
        append_message_delivery_signal_receipt(
            &conn,
            first_message_id,
            recipient_id,
            "filesystem_signal",
            "retry-path-digest",
            1_700_000_000_000_001,
        )
        .expect("idempotent receipt retry");

        let first = message_delivery_receipt_from_conn(&conn, project_id, first_message_id)
            .expect("read signaled first message");
        assert_eq!(first.recipients[0].signal_receipts.len(), 1);
        assert_eq!(
            first.recipients[0].signal_receipts[0].signal_path_digest,
            "first-path-digest"
        );

        let second = message_delivery_receipt_from_conn(&conn, project_id, second_message_id)
            .expect("read un-signaled second message");
        assert!(
            second.recipients[0].signal_receipts.is_empty(),
            "a recipient's latest signal must not be attributed to another message"
        );

        conn.execute_sync(
            "UPDATE message_recipients SET ack_ts = ?1 WHERE message_id = ?2 AND agent_id = ?3",
            &[
                Value::BigInt(1_700_000_000_000_100),
                Value::BigInt(first_message_id),
                Value::BigInt(recipient_id),
            ],
        )
        .expect("acknowledge first message");
        let acknowledgement_rows = conn
            .query_sync(
                "SELECT ack_ts FROM message_recipients WHERE message_id = ?1 AND agent_id = ?2",
                &[Value::BigInt(first_message_id), Value::BigInt(recipient_id)],
            )
            .expect("read raw acknowledgement");
        let raw_acknowledged_ts: Option<i64> = acknowledgement_rows[0]
            .get_named("ack_ts")
            .expect("decode raw acknowledgement");
        assert_eq!(raw_acknowledged_ts, Some(1_700_000_000_000_100));
        let acknowledged = message_delivery_receipt_from_conn(&conn, project_id, first_message_id)
            .expect("read acknowledged receipt");
        assert_eq!(
            acknowledged.recipients[0].acknowledged_ts,
            Some(1_700_000_000_000_100)
        );
    }

    // ── update_message_thread_id tests ───────────────────────────────

    #[test]
    fn update_thread_id_empty_target_returns_false() {
        let conn = test_conn();
        assert!(!update_message_thread_id(&conn, 1, "").unwrap());
        assert!(!update_message_thread_id(&conn, 1, "   ").unwrap());
    }

    #[test]
    fn update_thread_id_nonexistent_message_returns_not_found() {
        let conn = test_conn();
        let err = update_message_thread_id(&conn, 99999, "new-thread").unwrap_err();
        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Message",
                    ..
                }
            ),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn update_thread_id_same_value_returns_false() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "original-thread");

        let result = update_message_thread_id(&conn, mid, "original-thread").unwrap();
        assert!(
            !result,
            "should return false when thread_id is already the target"
        );
    }

    #[test]
    fn update_thread_id_different_value_returns_true() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "old-thread");

        let result = update_message_thread_id(&conn, mid, "new-thread").unwrap();
        assert!(result, "should return true when thread_id changes");

        // Verify the update persisted
        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(mid)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "new-thread");
    }

    #[test]
    fn update_thread_id_trims_whitespace() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "old");

        let result = update_message_thread_id(&conn, mid, "  new-thread  ").unwrap();
        assert!(result);

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(mid)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "new-thread");
    }

    // ── dispatch_root_message tests ──────────────────────────────────

    #[test]
    fn dispatch_root_message_no_project_returns_not_found() {
        let conn = test_conn();
        let err = dispatch_root_message(&conn, "SomeAgent", "Hello", "Body", "normal", None, &[])
            .unwrap_err();
        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Project",
                    ..
                }
            ),
            "expected Project NotFound, got {err:?}"
        );
    }

    #[test]
    fn dispatch_root_message_auto_registers_sender() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        // NewAgent doesn't exist yet — dispatch should auto-register
        let msg_id = dispatch_root_message(
            &conn,
            "NewAgent",
            "Auto-register test",
            "Should auto-register the sender",
            "normal",
            None,
            &[],
        )
        .unwrap();

        assert!(msg_id > 0);

        // Verify agent was created
        let rows = conn
            .query_sync(
                "SELECT name, program FROM agents WHERE name = 'NewAgent'",
                &[],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("agent should exist");
        assert_eq!(row.get_named::<String>("program").unwrap(), "tui-overseer");
    }

    /// GH#227: the TUI compose dispatch runs inside the server process and
    /// must invalidate the process-wide search cache after committing, like
    /// every other delivery path — otherwise cached pre-delivery result sets
    /// keep answering identical queries with false-negatives for the TTL.
    #[test]
    fn gh227_dispatch_root_message_invalidates_search_cache() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let epoch_before = crate::search_service::global_search_cache_epoch_for_tests();
        let msg_id = dispatch_root_message(
            &conn,
            "ComposeAgent",
            "Cache invalidation test",
            "Body",
            "normal",
            None,
            &[],
        )
        .unwrap();
        assert!(msg_id > 0);
        assert!(
            crate::search_service::global_search_cache_epoch_for_tests() > epoch_before,
            "TUI-composed message must bump the search cache epoch (GH#227)"
        );
    }

    #[test]
    fn dispatch_root_message_uses_existing_sender() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _aid = insert_agent(&conn, pid, "ExistingAgent");

        let msg_id = dispatch_root_message(
            &conn,
            "ExistingAgent",
            "Existing agent test",
            "Body",
            "high",
            Some("thread-123"),
            &[],
        )
        .unwrap();

        assert!(msg_id > 0);

        // Verify only one agent with that name
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents WHERE name = 'ExistingAgent'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(cnt, 1, "should not create duplicate agent");
    }

    #[test]
    fn dispatch_root_message_with_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id = dispatch_root_message(
            &conn,
            "Agent",
            "Thread test",
            "Body",
            "normal",
            Some("br-42"),
            &[],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "br-42");
    }

    #[test]
    fn dispatch_root_message_trims_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id = dispatch_root_message(
            &conn,
            "Agent",
            "Thread trim test",
            "Body",
            "normal",
            Some("  br-100  "),
            &[],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "br-100");
    }

    #[test]
    fn dispatch_root_message_without_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id =
            dispatch_root_message(&conn, "Agent", "No thread", "Body", "normal", None, &[])
                .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("message should exist");
        // thread_id should be NULL
        assert!(row.get_named::<String>("thread_id").is_err());
    }

    #[test]
    fn dispatch_root_message_links_recipients() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _r1 = insert_agent(&conn, pid, "Recipient1");
        let _r2 = insert_agent(&conn, pid, "Recipient2");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Multi-recipient",
            "Body",
            "normal",
            None,
            &[
                ("Recipient1".to_string(), "to".to_string()),
                ("Recipient2".to_string(), "cc".to_string()),
            ],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients WHERE message_id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(cnt, 2, "should have 2 recipients");

        let message_rows = conn
            .query_sync(
                "SELECT recipients_json FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let recipients_json = message_rows
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<String>("recipients_json").ok())
            .unwrap();
        assert!(recipients_json.contains("Recipient1"));
        assert!(recipients_json.contains("Recipient2"));
    }

    #[test]
    fn sync_message_recipients_json_keeps_orphaned_recipient_rows_visible() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient1");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Orphaned recipient",
            "Body",
            "normal",
            None,
            &[("Recipient1".to_string(), "to".to_string())],
        )
        .unwrap();

        conn.execute_sync(
            "DELETE FROM agents WHERE id = ? AND project_id = ?",
            &[Value::BigInt(recipient_id), Value::BigInt(pid)],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT OR IGNORE INTO message_recipients \
             (message_id, agent_id, kind, read_ts, ack_ts) \
             VALUES (?, ?, 'to', NULL, NULL)",
            &[Value::BigInt(msg_id), Value::BigInt(recipient_id)],
        )
        .unwrap();

        sync_message_recipients_json(&conn, msg_id).expect("sync recipients_json");

        let message_rows = conn
            .query_sync(
                "SELECT recipients_json FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let recipients_json = message_rows
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<String>("recipients_json").ok())
            .unwrap();
        assert!(
            recipients_json.contains(&format!("[unknown-agent-{recipient_id}]")),
            "orphaned recipient placeholder missing from {recipients_json}"
        );
    }

    #[test]
    fn dispatch_root_message_duplicate_recipient_inserted_once() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _r1 = insert_agent(&conn, pid, "Recipient1");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Duplicate recipient",
            "Body",
            "normal",
            None,
            &[
                ("Recipient1".to_string(), "to".to_string()),
                ("Recipient1".to_string(), "cc".to_string()),
            ],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT kind FROM message_recipients WHERE message_id = ? ORDER BY kind",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "duplicate recipients should be de-duplicated"
        );
        let kind = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("kind").ok())
            .unwrap();
        assert_eq!(kind, "to", "first occurrence should win");
    }

    #[test]
    fn dispatch_root_message_reuses_sender_case_insensitively() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "BlueLake");

        let msg_id = dispatch_root_message(
            &conn,
            "bluelake",
            "Sender case fold",
            "Body",
            "normal",
            None,
            &[],
        )
        .unwrap();

        let sender_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents \
                 WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE",
                &[Value::BigInt(pid), Value::Text("BlueLake".to_string())],
            )
            .unwrap();
        let sender_count = sender_rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(sender_count, 1, "sender lookup should be case-insensitive");

        let msg_rows = conn
            .query_sync(
                "SELECT sender_id FROM messages WHERE id = ?1",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let actual_sender_id = msg_rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("sender_id").ok())
            .unwrap();
        assert_eq!(actual_sender_id, sender_id);
    }

    #[test]
    fn dispatch_root_message_resolves_recipients_case_insensitively() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "BlueLake");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Recipient case fold",
            "Body",
            "normal",
            None,
            &[("bluelake".to_string(), "to".to_string())],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT agent_id, kind FROM message_recipients WHERE message_id = ?1",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("recipient row should exist");
        assert_eq!(row.get_named::<i64>("agent_id").unwrap(), recipient_id);
        assert_eq!(row.get_named::<String>("kind").unwrap(), "to");
    }

    #[test]
    fn dispatch_root_message_unknown_recipient_returns_not_found_and_rolls_back() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let err = dispatch_root_message(
            &conn,
            "Sender",
            "Unknown recipient",
            "Body",
            "normal",
            None,
            &[("NonexistentAgent".to_string(), "to".to_string())],
        )
        .expect_err("unknown recipient should fail");

        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Agent",
                    ..
                }
            ),
            "expected agent not found, got {err:?}"
        );

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE subject = 'Unknown recipient'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(
            cnt, 0,
            "message insert should roll back on unknown recipient"
        );
    }

    #[test]
    fn dispatch_root_message_recipient_insert_error_rolls_back_message() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _recipient = insert_agent(&conn, pid, "Recipient1");

        conn.execute_raw(
            "CREATE TRIGGER fail_message_recipient_insert \
             BEFORE INSERT ON message_recipients \
             BEGIN \
                 SELECT RAISE(ABORT, 'forced recipient insert failure'); \
             END;",
        )
        .expect("install failing recipient trigger");

        let err = dispatch_root_message(
            &conn,
            "Sender",
            "Rollback recipient error",
            "Body",
            "normal",
            None,
            &[("Recipient1".to_string(), "to".to_string())],
        )
        .expect_err("recipient insert should fail when table is missing");
        assert!(matches!(err, DbError::Sqlite(_)));

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE subject = 'Rollback recipient error'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(
            cnt, 0,
            "message insert should roll back on recipient failure"
        );
    }

    #[test]
    fn dispatch_root_message_stores_importance() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id =
            dispatch_root_message(&conn, "Agent", "Urgent", "Body", "urgent", None, &[]).unwrap();

        let rows = conn
            .query_sync(
                "SELECT importance FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let importance = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("importance").ok())
            .unwrap();
        assert_eq!(importance, "urgent");
    }

    #[test]
    fn mark_messages_read_batch_sync_updates_rows_and_inbox_stats() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient");

        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts, recipients_json, attachments) \
             VALUES (?1, ?2, 'ack', 'body', 'normal', 1, NULL, 1000001, '[]', '[]')",
            &[Value::BigInt(pid), Value::BigInt(sender_id)],
        )
        .unwrap();
        let ack_message_id = conn
            .query_sync("SELECT last_insert_rowid() AS id", &[])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("id").ok())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(ack_message_id), Value::BigInt(recipient_id)],
        )
        .unwrap();

        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts, recipients_json, attachments) \
             VALUES (?1, ?2, 'plain', 'body', 'normal', 0, NULL, 1000002, '[]', '[]')",
            &[Value::BigInt(pid), Value::BigInt(sender_id)],
        )
        .unwrap();
        let plain_message_id = conn
            .query_sync("SELECT last_insert_rowid() AS id", &[])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("id").ok())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(plain_message_id), Value::BigInt(recipient_id)],
        )
        .unwrap();

        let batch = mark_messages_read_batch_sync_conn(
            &conn,
            recipient_id,
            &[plain_message_id, ack_message_id, ack_message_id],
        )
        .unwrap()
        .expect("non-empty batch should return the written rows");
        assert_eq!(
            batch.message_ids,
            vec![ack_message_id, plain_message_id],
            "batch helper should report the message IDs it updated"
        );

        let rows = conn
            .query_sync(
                "SELECT message_id, read_ts, ack_ts FROM message_recipients \
                 WHERE agent_id = ?1 ORDER BY message_id",
                &[Value::BigInt(recipient_id)],
            )
            .unwrap();
        assert_eq!(rows.len(), 2, "expected two recipient rows");

        let first = rows.first().expect("first recipient row");
        let second = rows.get(1).expect("second recipient row");
        let first_message_id = first.get_named::<i64>("message_id").unwrap();
        let second_message_id = second.get_named::<i64>("message_id").unwrap();

        for row in [first, second] {
            let read_ts = row
                .get_named::<i64>("read_ts")
                .expect("read_ts should be populated after sync batch mark-read");
            assert_eq!(
                read_ts, batch.read_ts,
                "batch helper should report the timestamp it wrote"
            );
        }

        let ack_row = if first_message_id == ack_message_id {
            first
        } else {
            second
        };
        let plain_row = if second_message_id == plain_message_id {
            second
        } else {
            first
        };
        assert!(
            ack_row.get_named::<i64>("ack_ts").is_err(),
            "ack_required message should remain pending acknowledgement after mark-read"
        );
        assert!(
            plain_row.get_named::<i64>("ack_ts").is_err(),
            "non-ack-required message should keep ack_ts NULL"
        );

        let stats_row = conn
            .query_sync(
                "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = ?1",
                &[Value::BigInt(recipient_id)],
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("inbox_stats row should exist");
        assert_eq!(stats_row.get_named::<i64>("total_count").unwrap(), 2);
        assert_eq!(stats_row.get_named::<i64>("unread_count").unwrap(), 0);
        assert_eq!(stats_row.get_named::<i64>("ack_pending_count").unwrap(), 1);
    }

    #[test]
    fn mark_messages_read_batch_sync_conn_returns_none_when_no_rows_match() {
        let conn = test_conn();

        let read_ts = mark_messages_read_batch_sync_conn(&conn, 42, &[100, 101])
            .expect("no matching rows should still be a successful no-op");

        assert_eq!(read_ts, None);
    }

    #[test]
    fn mark_messages_read_batch_sync_conn_returns_none_when_rows_already_read() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient");
        let message_id = insert_message(&conn, pid, sender_id, "already-read");
        let existing_read_ts = 1_770_354_000_000_000;
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts) \
             VALUES (?1, ?2, 'to', ?3)",
            &[
                Value::BigInt(message_id),
                Value::BigInt(recipient_id),
                Value::BigInt(existing_read_ts),
            ],
        )
        .expect("insert already-read recipient row");

        let read_ts = mark_messages_read_batch_sync_conn(&conn, recipient_id, &[message_id])
            .expect("already-read rows should remain a successful no-op");

        assert_eq!(read_ts, None);
        let stored_read_ts = conn
            .query_sync(
                "SELECT read_ts FROM message_recipients WHERE message_id = ?1 AND agent_id = ?2",
                &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
            )
            .expect("select read_ts")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("read_ts").ok())
            .expect("read_ts should remain populated");
        assert_eq!(stored_read_ts, existing_read_ts);
    }

    #[test]
    fn mark_messages_read_batch_sync_conn_reports_only_rows_it_updates() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient");
        let updated_message_id = insert_message(&conn, pid, sender_id, "updated");
        let already_read_message_id = insert_message(&conn, pid, sender_id, "already-read");
        let missing_message_id = 99_999;
        let existing_read_ts = 1_770_354_000_000_000;
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[
                Value::BigInt(updated_message_id),
                Value::BigInt(recipient_id),
            ],
        )
        .expect("insert unread recipient row");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts) \
             VALUES (?1, ?2, 'to', ?3)",
            &[
                Value::BigInt(already_read_message_id),
                Value::BigInt(recipient_id),
                Value::BigInt(existing_read_ts),
            ],
        )
        .expect("insert already-read recipient row");

        let batch = mark_messages_read_batch_sync_conn(
            &conn,
            recipient_id,
            &[
                updated_message_id,
                already_read_message_id,
                missing_message_id,
            ],
        )
        .expect("partial update should succeed")
        .expect("one row should be updated");

        assert_eq!(batch.message_ids, vec![updated_message_id]);
    }

    #[test]
    fn mark_messages_read_batch_sync_empty_ids_does_not_create_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sqlite_path = dir.path().join("mailbox.sqlite3");
        let sqlite_path_str = sqlite_path
            .to_str()
            .expect("temporary sqlite path should be valid UTF-8");

        let read_ts = mark_messages_read_batch_sync(sqlite_path_str, 42, &[])
            .expect("empty batch is a no-op");
        assert_eq!(read_ts, None);

        assert!(
            !sqlite_path.exists(),
            "empty mark-read batch should not create or open a live DB"
        );
    }
}
