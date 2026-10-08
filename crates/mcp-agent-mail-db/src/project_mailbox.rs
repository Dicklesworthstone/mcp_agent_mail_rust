//! Project-addressed shared mailboxes (GH#282).
//!
//! A message addressed to `project:<slug>` (or `project:<human_key>`) is
//! persisted ONCE: one `messages` row plus one `project_mailbox_deliveries`
//! row. Nothing is fanned out per agent — not at send time and not when an
//! agent registers — so this is not the deliberately rejected `broadcast`.
//!
//! # Visibility
//!
//! A project delivery appears in the inbox of an agent (the *viewer*) when:
//!
//! - the viewer is registered in the delivery's project and is not retired;
//! - the viewer is not the sender;
//! - the viewer registered at or before the message was sent
//!   (`inception_ts <= created_ts`). A shared mailbox delivers to the agents
//!   present when a message is sent, like a mailing list delivers to its
//!   subscribers; an agent that joins later is not handed the project's whole
//!   backlog as unread mail or overdue acknowledgements. Project-wide history
//!   stays readable through threads, `fetch_topic` and search;
//! - the viewer has not opted out of unsolicited mail (`contact_policy` of
//!   `block_all`); and
//! - the viewer is not also a direct recipient of the same message. The direct
//!   delivery wins, so one message never appears twice in one inbox.
//!
//! # Read and acknowledgement state
//!
//! Per-agent state lives in `project_mailbox_receipts`, created lazily by the
//! write paths (`mark_message_read`, `acknowledge_message`, the read receipt
//! that follows `fetch_inbox`). A plain inbox read never writes, so reading
//! the shared mailbox cannot manufacture another agent's activity.
//!
//! # Delivery cursor
//!
//! Restart-safe monitors page `inbox_delivery_events` by its global `seq`. A
//! project delivery appends ONE event there, for the pseudo-recipient
//! [`PROJECT_MAILBOX_EVENT_AGENT_ID`], in the send transaction. Each viewer's
//! event page merges its own events with the visible project events, so one
//! cursor covers both without a per-agent row.

use crate::DbConn;
use crate::error::DbError;
use crate::models::MessageRow;
use crate::queries::{InboxRow, UNKNOWN_SENDER_DISPLAY};
use crate::sync::{InboxBodyPolicy, InboxFetchOptions};
use sqlmodel_core::Value;

/// Recipient `kind` reported for a project mailbox delivery. It never appears
/// in `message_recipients`.
pub const PROJECT_MAILBOX_KIND: &str = "project";

/// Address prefix that names a project's shared mailbox.
pub const PROJECT_MAILBOX_ADDRESS_PREFIX: &str = "project:";

/// `inbox_delivery_events.agent_id` of a project delivery's single cursor
/// event. Agent ids start at 1, so 0 never names a real recipient.
pub const PROJECT_MAILBOX_EVENT_AGENT_ID: i64 = 0;

/// Append a project delivery's cursor event. Binds: project id, message id,
/// delivered timestamp. Idempotent for a re-driven insert.
pub const INSERT_PROJECT_MAILBOX_EVENT_SQL: &str = "INSERT OR IGNORE INTO inbox_delivery_events \
     (project_id, agent_id, message_id, kind, delivered_ts) VALUES (?, 0, ?, 'project', ?)";

const MAX_IN_CLAUSE_ITEMS: usize = 500;

/// SQL predicate selecting the deliveries (`d`, joined to their message `m`)
/// that the agent `viewer` may see. See the module docs for the rule. A macro
/// so `concat!` can embed it in other `const` SQL (the retention predicate).
macro_rules! visible_to_viewer_sql {
    () => {
        "d.project_id = viewer.project_id \
         AND m.project_id = d.project_id \
         AND m.sender_id <> viewer.id \
         AND viewer.retired_at IS NULL \
         AND viewer.inception_ts <= m.created_ts \
         AND LOWER(COALESCE(viewer.contact_policy, 'auto')) <> 'block_all' \
         AND NOT EXISTS (SELECT 1 FROM message_recipients direct \
                         WHERE direct.message_id = d.message_id AND direct.agent_id = viewer.id)"
    };
}
pub(crate) use visible_to_viewer_sql;

const VISIBLE_TO_VIEWER_SQL: &str = visible_to_viewer_sql!();

/// The project identifier named by a shared-mailbox address, or `None` when
/// `raw` is not one.
///
/// `project:<project>#<Agent>` is the Python-era project-qualified *agent*
/// address (GH#335), so an identifier containing `#` is never a mailbox. The
/// returned identifier may be empty; callers reject that explicitly.
#[must_use]
pub fn parse_project_mailbox_address(raw: &str) -> Option<&str> {
    let rest = raw.trim().strip_prefix(PROJECT_MAILBOX_ADDRESS_PREFIX)?;
    (!rest.contains('#')).then(|| rest.trim())
}

/// The canonical shared-mailbox address of a project.
#[must_use]
pub fn project_mailbox_address(project_slug: &str) -> String {
    format!("{PROJECT_MAILBOX_ADDRESS_PREFIX}{project_slug}")
}

/// Whether a database error only says the shared-mailbox tables do not exist.
///
/// That is a mailbox opened read-only before its schema was upgraded. It
/// cannot hold project deliveries, so readers treat it as empty.
#[must_use]
pub fn is_missing_project_mailbox_table_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("no such table") && lowered.contains("project_mailbox_")
}

/// Shared-mailbox inbox rows for one agent, newest first, with the same
/// filter set as the direct inbox. Each row carries
/// [`PROJECT_MAILBOX_KIND`] and the agent's own lazily-recorded read/ack state.
pub fn fetch_project_mailbox_rows_from_conn(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    let body_select = match options.body_policy {
        InboxBodyPolicy::Full => "m.body_md",
        InboxBodyPolicy::MetadataOnly => "'' AS body_md",
    };
    let mut sql = format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.topic, m.subject, {body_select}, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, \
                pr.read_ts AS read_ts, pr.ack_ts AS ack_ts \
         FROM project_mailbox_deliveries d \
         JOIN messages m ON m.id = d.message_id \
         JOIN agents viewer ON viewer.id = ? \
         LEFT JOIN agents s ON s.id = m.sender_id \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE d.project_id = ? AND {VISIBLE_TO_VIEWER_SQL}"
    );
    let mut params = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];
    if options.urgent_only {
        sql.push_str(" AND m.importance IN ('high', 'urgent')");
    }
    if options.unread_only {
        sql.push_str(" AND pr.read_ts IS NULL");
    }
    if options.ack_required_only {
        sql.push_str(" AND m.ack_required = 1 AND pr.ack_ts IS NULL");
    }
    if let Some(threshold) = options.ack_overdue_before {
        sql.push_str(" AND m.ack_required = 1 AND pr.ack_ts IS NULL AND m.created_ts < ?");
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
    sql.push_str(" ORDER BY m.created_ts DESC, m.id DESC LIMIT ?");
    params.push(Value::BigInt(limit_i64));

    let rows = match conn.query_sync(&sql, &params) {
        Ok(rows) => rows,
        Err(error) if is_missing_project_mailbox_table_error(&error.to_string()) => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(DbError::Sqlite(error.to_string())),
    };

    let column = |row: &sqlmodel_core::Row, index: usize| -> Result<Value, DbError> {
        row.get(index).cloned().ok_or_else(|| {
            DbError::Sqlite(format!("project mailbox row is missing column {index}"))
        })
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let text = |index: usize| -> Result<String, DbError> {
            match column(&row, index)? {
                Value::Text(text) => Ok(text),
                Value::Null => Ok(String::new()),
                other => Err(DbError::Sqlite(format!(
                    "project mailbox column {index} is not text: {other:?}"
                ))),
            }
        };
        let optional_text = |index: usize| -> Result<Option<String>, DbError> {
            match column(&row, index)? {
                Value::Null => Ok(None),
                Value::Text(text) => Ok(Some(text)),
                other => Err(DbError::Sqlite(format!(
                    "project mailbox column {index} is not text: {other:?}"
                ))),
            }
        };
        let integer = |index: usize| -> Result<Option<i64>, DbError> {
            match column(&row, index)? {
                Value::Null => Ok(None),
                Value::BigInt(value) => Ok(Some(value)),
                Value::Int(value) => Ok(Some(i64::from(value))),
                other => Err(DbError::Sqlite(format!(
                    "project mailbox column {index} is not an integer: {other:?}"
                ))),
            }
        };
        let required = |index: usize| -> Result<i64, DbError> {
            integer(index)?
                .ok_or_else(|| DbError::Sqlite(format!("project mailbox column {index} is NULL")))
        };
        out.push(InboxRow {
            message: MessageRow {
                id: Some(required(0)?),
                project_id: required(1)?,
                sender_id: required(2)?,
                thread_id: optional_text(3)?,
                topic: optional_text(4)?,
                subject: text(5)?,
                body_md: text(6)?,
                importance: text(7)?,
                ack_required: required(8)?,
                created_ts: required(9)?,
                recipients_json: text(10)?,
                attachments: text(11)?,
            },
            kind: PROJECT_MAILBOX_KIND.to_string(),
            sender_name: text(12)?,
            read_ts: integer(13)?,
            ack_ts: integer(14)?,
        });
    }
    Ok(out)
}

/// Shared-mailbox rows for the product-bus inbox.
///
/// Covers every project linked to a product, for the agents named
/// `agent_name` there. Columns match the product inbox's indexed decoding:
/// message columns, then kind, sender name, read and ack timestamps. Binds:
/// unknown-sender display, agent name, product id; the caller appends
/// filters, ordering and the limit.
#[must_use]
pub fn product_inbox_select_sql(body_select: &str) -> String {
    format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.topic, m.subject, {body_select}, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                '{PROJECT_MAILBOX_KIND}' AS kind, COALESCE(s.name, ?) AS sender_name, \
                pr.read_ts, pr.ack_ts \
         FROM product_project_links ppl \
         JOIN agents viewer ON viewer.project_id = ppl.project_id AND viewer.name = ? COLLATE NOCASE \
         JOIN project_mailbox_deliveries d ON d.project_id = ppl.project_id \
         JOIN messages m ON m.id = d.message_id \
         LEFT JOIN agents s ON s.id = m.sender_id \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE ppl.product_id = ? AND {VISIBLE_TO_VIEWER_SQL}"
    )
}

/// Project cursor events visible to a viewer, oldest first.
///
/// These are the `inbox_delivery_events` rows of the
/// [`PROJECT_MAILBOX_EVENT_AGENT_ID`], in the column shape of the recipient
/// event page. Binds: unknown-sender display, viewer agent id, project id,
/// cursor (`seq >`), limit.
#[must_use]
pub fn visible_events_sql() -> String {
    format!(
        "SELECT e.seq, e.message_id, e.kind, e.delivered_ts, m.subject, \
                COALESCE(sender.name, ?) AS sender_name, m.importance, m.ack_required \
         FROM inbox_delivery_events AS e \
         JOIN project_mailbox_deliveries AS d ON d.message_id = e.message_id \
         JOIN messages AS m ON m.id = e.message_id \
         JOIN agents AS viewer ON viewer.id = ? \
         LEFT JOIN agents AS sender ON sender.id = m.sender_id \
         WHERE e.project_id = ? AND e.agent_id = {PROJECT_MAILBOX_EVENT_AGENT_ID} \
           AND e.seq > ? AND {VISIBLE_TO_VIEWER_SQL} \
         ORDER BY e.seq ASC LIMIT ?"
    )
}

/// Merge direct and shared-mailbox inbox rows into one newest-first window of
/// at most `limit` rows.
#[must_use]
pub fn merge_inbox_rows(
    mut direct: Vec<InboxRow>,
    shared: Vec<InboxRow>,
    limit: usize,
) -> Vec<InboxRow> {
    if shared.is_empty() {
        return direct;
    }
    direct.extend(shared);
    direct.sort_by(|left, right| {
        right
            .message
            .created_ts
            .cmp(&left.message.created_ts)
            .then_with(|| right.message.id.cmp(&left.message.id))
    });
    direct.truncate(limit);
    direct
}

/// A visible project delivery: `(message_id, read_ts, ack_ts)` of the viewer.
type VisibleReceipt = (i64, Option<i64>, Option<i64>);

/// Message ids among `message_ids` that are project deliveries visible to
/// `agent_id`, together with the agent's stored receipt.
fn visible_receipts(
    conn: &impl crate::pool::SyncQuery,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<Vec<VisibleReceipt>, DbError> {
    let mut out = Vec::new();
    for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT d.message_id AS message_id, pr.read_ts AS read_ts, pr.ack_ts AS ack_ts \
             FROM project_mailbox_deliveries d \
             JOIN messages m ON m.id = d.message_id \
             JOIN agents viewer ON viewer.id = ? \
             LEFT JOIN project_mailbox_receipts pr \
                    ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
             WHERE d.message_id IN ({placeholders}) AND {VISIBLE_TO_VIEWER_SQL}"
        );
        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(Value::BigInt(agent_id));
        params.extend(chunk.iter().copied().map(Value::BigInt));
        let rows = match conn.query_sync(&sql, &params) {
            Ok(rows) => rows,
            Err(error) if is_missing_project_mailbox_table_error(&error.to_string()) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(DbError::Sqlite(error.to_string())),
        };
        for row in rows {
            let message_id = row
                .get_named::<i64>("message_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let read_ts = row.get_named::<Option<i64>>("read_ts").ok().flatten();
            let ack_ts = row.get_named::<Option<i64>>("ack_ts").ok().flatten();
            out.push((message_id, read_ts, ack_ts));
        }
    }
    Ok(out)
}

/// Which receipt field a write records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptUpdate {
    /// Set `read_ts` if unset.
    Read,
    /// Set `read_ts` and `ack_ts` if unset (an acknowledgement implies read).
    Acknowledge,
}

/// SQL statements that record a receipt for one `(message_id, agent_id)`.
///
/// They create the lazy row, then fill the requested timestamps without
/// overwriting existing ones. Exposed so the async pooled write paths run
/// exactly the same statements inside their own transactions.
#[must_use]
pub fn receipt_write_statements(
    update: ReceiptUpdate,
    message_id: i64,
    agent_id: i64,
    now: i64,
) -> [(&'static str, Vec<Value>); 2] {
    let insert = (
        "INSERT INTO project_mailbox_receipts (message_id, agent_id, read_ts, ack_ts) \
         VALUES (?, ?, NULL, NULL) ON CONFLICT DO NOTHING",
        vec![Value::BigInt(message_id), Value::BigInt(agent_id)],
    );
    let update = match update {
        ReceiptUpdate::Read => (
            "UPDATE project_mailbox_receipts SET read_ts = COALESCE(read_ts, ?) \
             WHERE message_id = ? AND agent_id = ?",
            vec![
                Value::BigInt(now),
                Value::BigInt(message_id),
                Value::BigInt(agent_id),
            ],
        ),
        ReceiptUpdate::Acknowledge => (
            "UPDATE project_mailbox_receipts \
             SET read_ts = COALESCE(read_ts, ?), ack_ts = COALESCE(ack_ts, ?) \
             WHERE message_id = ? AND agent_id = ?",
            vec![
                Value::BigInt(now),
                Value::BigInt(now),
                Value::BigInt(message_id),
                Value::BigInt(agent_id),
            ],
        ),
    };
    [insert, update]
}

/// The visibility check and stored receipt of one project delivery.
///
/// Decides whether `message_id` is a project delivery visible to `agent_id`
/// and reads the agent's receipt, for use inside a write transaction.
/// Returns `(sql, params)`; the result row (if any) has `read_ts` and
/// `ack_ts` columns.
#[must_use]
pub fn visible_receipt_query(agent_id: i64, message_id: i64) -> (String, Vec<Value>) {
    (
        format!(
            "SELECT pr.read_ts AS read_ts, pr.ack_ts AS ack_ts \
             FROM project_mailbox_deliveries d \
             JOIN messages m ON m.id = d.message_id \
             JOIN agents viewer ON viewer.id = ? \
             LEFT JOIN project_mailbox_receipts pr \
                    ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
             WHERE d.message_id = ? AND {VISIBLE_TO_VIEWER_SQL}"
        ),
        vec![Value::BigInt(agent_id), Value::BigInt(message_id)],
    )
}

/// Oldest-first ids of the visible project deliveries an agent has not read.
///
/// Optionally only those created at or before `older_than`. Returns
/// `(sql, params)` selecting at most `limit` `message_id` rows.
#[must_use]
pub fn unread_visible_query(
    agent_id: i64,
    project_id: i64,
    older_than: Option<i64>,
    limit: i64,
) -> (String, Vec<Value>) {
    let mut sql = format!(
        "SELECT d.message_id AS message_id \
         FROM project_mailbox_deliveries d \
         JOIN messages m ON m.id = d.message_id \
         JOIN agents viewer ON viewer.id = ? \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE d.project_id = ? AND pr.read_ts IS NULL AND {VISIBLE_TO_VIEWER_SQL}"
    );
    let mut params = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];
    if let Some(cutoff) = older_than {
        sql.push_str(" AND m.created_ts <= ?");
        params.push(Value::BigInt(cutoff));
    }
    sql.push_str(" ORDER BY d.message_id LIMIT ?");
    params.push(Value::BigInt(limit));
    (sql, params)
}

/// Result of a batch read receipt over project mailbox deliveries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMailboxReadBatch {
    pub read_ts: i64,
    pub message_ids: Vec<i64>,
}

/// Record read receipts for the visible, still-unread project deliveries
/// among `message_ids`. Ids that are not project deliveries visible to the
/// agent are ignored. Runs in one write transaction on `conn`.
pub fn mark_project_mailbox_read_batch_sync_conn(
    conn: &DbConn,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<Option<ProjectMailboxReadBatch>, DbError> {
    if message_ids.is_empty() {
        return Ok(None);
    }
    let mut unique = message_ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let read_ts = crate::now_micros();

    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let result = (|| -> Result<Vec<i64>, DbError> {
        let mut updated = Vec::new();
        for (message_id, existing_read, _) in visible_receipts(conn, agent_id, &unique)? {
            if existing_read.is_some() {
                continue;
            }
            for (sql, params) in
                receipt_write_statements(ReceiptUpdate::Read, message_id, agent_id, read_ts)
            {
                conn.execute_sync(sql, &params)
                    .map_err(|error| DbError::Sqlite(error.to_string()))?;
            }
            updated.push(message_id);
        }
        Ok(updated)
    })();
    match result {
        Ok(updated) => {
            conn.execute_sync("COMMIT", &[])
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            Ok((!updated.is_empty()).then_some(ProjectMailboxReadBatch {
                read_ts,
                message_ids: updated,
            }))
        }
        Err(error) => {
            let _ = conn.execute_sync("ROLLBACK", &[]);
            Err(error)
        }
    }
}

/// Open `sqlite_path` and run [`mark_project_mailbox_read_batch_sync_conn`].
pub fn mark_project_mailbox_read_batch_sync(
    sqlite_path: &str,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<Option<ProjectMailboxReadBatch>, DbError> {
    if message_ids.is_empty() {
        return Ok(None);
    }
    let conn = DbConn::open_file(sqlite_path.to_string())
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let _ = conn.execute_raw(&format!(
        "PRAGMA busy_timeout = {}",
        mcp_agent_mail_core::config::DB_RUNTIME_BUSY_TIMEOUT_MS
    ));
    let result = mark_project_mailbox_read_batch_sync_conn(&conn, agent_id, message_ids);
    crate::close_db_conn(conn, "mark_project_mailbox_read_batch_sync connection");
    result
}

/// Per-agent state of one project mailbox delivery, for delivery receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMailboxReceiptRow {
    pub agent_id: i64,
    pub agent_name: String,
    pub read_ts: Option<i64>,
    pub ack_ts: Option<i64>,
}

/// One project delivery and every agent that can see it, with its receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMailboxDeliveryReceipt {
    pub project_id: i64,
    /// `to` or `cc`.
    pub kind: String,
    pub delivered_ts: i64,
    pub agents: Vec<ProjectMailboxReceiptRow>,
}

/// The project delivery of `message_id`, if any, with every agent that can
/// see it and that agent's receipt state.
pub fn project_mailbox_delivery_from_conn(
    conn: &impl crate::pool::SyncQuery,
    message_id: i64,
) -> Result<Option<ProjectMailboxDeliveryReceipt>, DbError> {
    let delivery = match conn.query_sync(
        "SELECT project_id, kind, delivered_ts FROM project_mailbox_deliveries WHERE message_id = ?",
        &[Value::BigInt(message_id)],
    ) {
        Ok(rows) => rows,
        Err(error) if is_missing_project_mailbox_table_error(&error.to_string()) => {
            return Ok(None);
        }
        Err(error) => return Err(DbError::Sqlite(error.to_string())),
    };
    let Some(row) = delivery.into_iter().next() else {
        return Ok(None);
    };
    let project_id = row
        .get_named::<i64>("project_id")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let kind = row
        .get_named::<String>("kind")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let delivered_ts = row
        .get_named::<i64>("delivered_ts")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;

    let sql = format!(
        "SELECT viewer.id AS agent_id, viewer.name AS agent_name, \
                pr.read_ts AS read_ts, pr.ack_ts AS ack_ts \
         FROM project_mailbox_deliveries d \
         JOIN messages m ON m.id = d.message_id \
         JOIN agents viewer ON viewer.project_id = d.project_id \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE d.message_id = ? AND {VISIBLE_TO_VIEWER_SQL} \
         ORDER BY viewer.name COLLATE NOCASE, viewer.id"
    );
    let rows = conn
        .query_sync(&sql, &[Value::BigInt(message_id)])
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let mut agents = Vec::with_capacity(rows.len());
    for row in rows {
        agents.push(ProjectMailboxReceiptRow {
            agent_id: row
                .get_named::<i64>("agent_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?,
            agent_name: row
                .get_named::<String>("agent_name")
                .map_err(|error| DbError::Sqlite(error.to_string()))?,
            read_ts: row.get_named::<Option<i64>>("read_ts").ok().flatten(),
            ack_ts: row.get_named::<Option<i64>>("ack_ts").ok().flatten(),
        });
    }
    Ok(Some(ProjectMailboxDeliveryReceipt {
        project_id,
        kind,
        delivered_ts,
        agents,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn test_conn() -> DbConn {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
            .expect("apply PRAGMAs");
        let cx = asupersync::Cx::for_testing();
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            schema::migrate_to_latest_base(&cx, &conn)
                .await
                .into_result()
                .expect("init schema migrations");
        });
        conn
    }

    fn insert(conn: &DbConn, sql: &str, params: &[Value]) -> i64 {
        conn.execute_sync(sql, params).expect("insert");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("last id")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("id").ok())
            .expect("row id")
    }

    fn agent(conn: &DbConn, project_id: i64, name: &str, inception_ts: i64) -> i64 {
        insert(
            conn,
            "INSERT INTO agents (project_id, name, program, model, task_description, \
             inception_ts, last_active_ts) VALUES (?, ?, 'test', 'test', '', ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::Text(name.to_string()),
                Value::BigInt(inception_ts),
                Value::BigInt(inception_ts),
            ],
        )
    }

    fn project_message(conn: &DbConn, project_id: i64, sender_id: i64, created_ts: i64) -> i64 {
        let message_id = insert(
            conn,
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, \
             ack_required, created_ts, recipients_json) \
             VALUES (?, ?, 'shared', 'body', 'normal', 1, ?, '{\"to\":[\"project:demo\"]}')",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                Value::BigInt(created_ts),
            ],
        );
        conn.execute_sync(
            "INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts) \
             VALUES (?, ?, 'to', ?)",
            &[
                Value::BigInt(message_id),
                Value::BigInt(project_id),
                Value::BigInt(created_ts),
            ],
        )
        .expect("insert delivery");
        message_id
    }

    fn options() -> InboxFetchOptions<'static> {
        InboxFetchOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            topic: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        }
    }

    fn visible_ids(conn: &DbConn, project_id: i64, agent_id: i64) -> Vec<i64> {
        fetch_project_mailbox_rows_from_conn(conn, project_id, agent_id, None, 50, options())
            .expect("project mailbox rows")
            .into_iter()
            .map(|row| {
                assert_eq!(row.kind, PROJECT_MAILBOX_KIND);
                row.message.id.expect("message id")
            })
            .collect()
    }

    #[test]
    fn parses_only_mailbox_addresses() {
        assert_eq!(
            parse_project_mailbox_address("project:backend"),
            Some("backend")
        );
        assert_eq!(
            parse_project_mailbox_address("  project: /data/app "),
            Some("/data/app")
        );
        assert_eq!(parse_project_mailbox_address("project:"), Some(""));
        assert_eq!(
            parse_project_mailbox_address("project:backend#BlueLake"),
            None
        );
        assert_eq!(parse_project_mailbox_address("BlueLake"), None);
        assert_eq!(parse_project_mailbox_address("BlueLake@backend"), None);
        assert_eq!(project_mailbox_address("backend"), "project:backend");
    }

    #[test]
    fn visibility_follows_membership_registration_policy_and_direct_delivery() {
        let conn = test_conn();
        let project = insert(
            &conn,
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('demo', '/demo', 1)",
            &[],
        );
        let other_project = insert(
            &conn,
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('other', '/other', 1)",
            &[],
        );
        let sender = agent(&conn, project, "GreenCastle", 100);
        let reader = agent(&conn, project, "BlueLake", 100);
        let blocked = agent(&conn, project, "OrangeCreek", 100);
        let retired = agent(&conn, project, "RedStone", 100);
        let direct = agent(&conn, project, "PinkPond", 100);
        let late = agent(&conn, project, "PurpleHill", 5_000);
        let outsider = agent(&conn, other_project, "BlackRiver", 100);
        conn.execute_sync(
            "UPDATE agents SET contact_policy = 'block_all' WHERE id = ?",
            &[Value::BigInt(blocked)],
        )
        .expect("block_all");
        conn.execute_sync(
            "UPDATE agents SET retired_at = 900 WHERE id = ?",
            &[Value::BigInt(retired)],
        )
        .expect("retire");

        let message = project_message(&conn, project, sender, 1_000);
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, 'to')",
            &[Value::BigInt(message), Value::BigInt(direct)],
        )
        .expect("direct recipient");

        assert_eq!(visible_ids(&conn, project, reader), vec![message]);
        for hidden in [sender, blocked, retired, direct, late] {
            assert!(
                visible_ids(&conn, project, hidden).is_empty(),
                "agent {hidden} must not see the project delivery"
            );
        }
        assert!(
            visible_ids(&conn, other_project, outsider).is_empty(),
            "the other project has no delivery"
        );
        assert!(
            visible_ids(&conn, project, outsider).is_empty(),
            "naming the wrong project cannot widen visibility"
        );

        let delivery = project_mailbox_delivery_from_conn(&conn, message)
            .expect("delivery receipt")
            .expect("message has a project delivery");
        assert_eq!(delivery.kind, "to");
        assert_eq!(
            delivery
                .agents
                .iter()
                .map(|agent| agent.agent_id)
                .collect::<Vec<_>>(),
            vec![reader]
        );
    }

    #[test]
    fn read_batch_records_only_visible_unread_receipts() {
        let conn = test_conn();
        let project = insert(
            &conn,
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('demo', '/demo', 1)",
            &[],
        );
        let sender = agent(&conn, project, "GreenCastle", 100);
        let reader = agent(&conn, project, "BlueLake", 100);
        let first = project_message(&conn, project, sender, 1_000);
        let second = project_message(&conn, project, sender, 2_000);

        assert_eq!(
            mark_project_mailbox_read_batch_sync_conn(&conn, sender, &[first, second])
                .expect("sender batch"),
            None,
            "the sender has no receipt to record"
        );
        let batch = mark_project_mailbox_read_batch_sync_conn(&conn, reader, &[first, 99_999])
            .expect("reader batch")
            .expect("one receipt recorded");
        assert_eq!(batch.message_ids, vec![first]);
        assert_eq!(
            mark_project_mailbox_read_batch_sync_conn(&conn, reader, &[first])
                .expect("repeat batch"),
            None,
            "a second read is a no-op"
        );

        let mut unread = options();
        unread.unread_only = true;
        let rows = fetch_project_mailbox_rows_from_conn(&conn, project, reader, None, 50, unread)
            .expect("unread rows");
        assert_eq!(
            rows.iter()
                .map(|row| row.message.id.expect("id"))
                .collect::<Vec<_>>(),
            vec![second]
        );
        let rows =
            fetch_project_mailbox_rows_from_conn(&conn, project, reader, None, 50, options())
                .expect("all rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message.id, Some(second), "newest first");
        assert_eq!(rows[1].read_ts, Some(batch.read_ts));
        assert_eq!(rows[1].ack_ts, None);
    }

    #[test]
    fn merge_keeps_one_newest_first_window() {
        let row = |id: i64, created_ts: i64, kind: &str| InboxRow {
            message: MessageRow {
                id: Some(id),
                created_ts,
                ..MessageRow::default()
            },
            kind: kind.to_string(),
            sender_name: String::new(),
            read_ts: None,
            ack_ts: None,
        };
        let merged = merge_inbox_rows(
            vec![row(4, 40, "to"), row(1, 10, "to")],
            vec![
                row(3, 30, PROJECT_MAILBOX_KIND),
                row(2, 20, PROJECT_MAILBOX_KIND),
            ],
            3,
        );
        assert_eq!(
            merged
                .iter()
                .map(|row| row.message.id.expect("id"))
                .collect::<Vec<_>>(),
            vec![4, 3, 2]
        );
        let direct_only = merge_inbox_rows(vec![row(1, 10, "to")], Vec::new(), 3);
        assert_eq!(direct_only.len(), 1);
    }

    #[test]
    fn missing_tables_read_as_an_empty_mailbox() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        assert!(
            fetch_project_mailbox_rows_from_conn(&conn, 1, 1, None, 10, options())
                .expect("missing tables are empty")
                .is_empty()
        );
        assert_eq!(
            project_mailbox_delivery_from_conn(&conn, 1).expect("missing tables"),
            None
        );
    }
}
