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
//! project delivery appends ONE event there with no agent recipient
//! (`agent_id IS NULL`), in the send transaction. Each viewer's
//! event page merges its own events with the visible project events, so one
//! cursor covers both without a per-agent row.

use crate::DbConn;
use crate::error::DbError;
#[cfg(test)]
use crate::models::MessageRow;
use crate::queries::InboxRow;
#[cfg(test)]
use crate::queries::UNKNOWN_SENDER_DISPLAY;
#[cfg(test)]
use crate::sync::InboxBodyPolicy;
use crate::sync::InboxFetchOptions;
use sqlmodel_core::Value;
use std::collections::HashMap;

/// Recipient `kind` reported for a project mailbox delivery. It never appears
/// in `message_recipients`.
pub const PROJECT_MAILBOX_KIND: &str = "project";

/// Address prefix that names a project's shared mailbox.
pub const PROJECT_MAILBOX_ADDRESS_PREFIX: &str = "project:";

/// Append a project delivery's cursor event. Binds: project id, message id,
/// delivered timestamp. Idempotent for a re-driven insert.
pub const INSERT_PROJECT_MAILBOX_EVENT_SQL: &str = "INSERT OR IGNORE INTO inbox_delivery_events \
     (project_id, agent_id, message_id, kind, delivered_ts) VALUES (?, NULL, ?, 'project', ?)";

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

/// Whether an error names the missing shared-delivery table specifically.
///
/// Missing receipts are NOT evidence of an empty mailbox: existing deliveries
/// may still require reads and acknowledgements. This classifier is also used
/// by product-inbox and event readers; never hide a receipt-schema failure.
#[must_use]
pub fn is_missing_project_mailbox_table_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    missing_table_name(&lowered) == Some("project_mailbox_deliveries")
}

fn missing_table_name(message: &str) -> Option<&str> {
    let (_, suffix) = message.split_once("no such table:")?;
    let name = suffix.split_whitespace().next()?;
    let name = name.strip_prefix("main.").unwrap_or(name);
    Some(name.trim_matches(['\'', '"', '`', '[', ']']))
}

/// Only a genuinely pre-upgrade shared schema may use the empty-mailbox
/// fallback. Run this catalog check on a missing-table error, not on healthy
/// reads. Surviving project delivery events or durable cursor state prove this
/// is an upgraded mailbox even when both shared tables are gone. An incomplete schema, failed
/// inspection or broken view stays an error; callers must not mistake it for
/// a successfully observed empty inbox.
pub(crate) fn allow_absent_shared_schema(
    conn: &impl crate::pool::SyncQuery,
    error: &str,
) -> Result<(), DbError> {
    let lowered = error.to_ascii_lowercase();
    if !matches!(
        missing_table_name(&lowered),
        Some("project_mailbox_deliveries" | "project_mailbox_receipts")
    ) {
        return Err(DbError::Sqlite(error.to_string()));
    }
    let objects = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE name COLLATE NOCASE \
             IN ('project_mailbox_deliveries', 'project_mailbox_receipts', 'inbox_delivery_events', 'inbox_delivery_cursor_state')",
            &[],
        )
        .map_err(|failure| DbError::Sqlite(failure.to_string()))?;
    let mut has_events = false;
    let mut has_cursor_state = false;
    for row in objects {
        let name = row
            .get_named::<String>("name")
            .map_err(|failure| DbError::Sqlite(failure.to_string()))?;
        if name.eq_ignore_ascii_case("inbox_delivery_events") {
            has_events = true;
            continue;
        }
        if name.eq_ignore_ascii_case("inbox_delivery_cursor_state") {
            has_cursor_state = true;
            continue;
        }
        // Do not echo a missing-table signature that an outer legacy reader
        // could catch again and turn into an empty result.
        return Err(DbError::Sqlite(
            "incomplete project mailbox schema; delivery and receipt tables must be repaired"
                .to_string(),
        ));
    }
    if has_events
        && !conn
            .query_sync(
                "SELECT seq FROM inbox_delivery_events WHERE kind = 'project' LIMIT 1",
                &[],
            )
            .map_err(|failure| DbError::Sqlite(failure.to_string()))?
            .is_empty()
    {
        // Schema authority is global: even a delivery in another project
        // rules out the pre-upgrade exception for these missing tables.
        // The explicit kind also recognizes historical agent_id=0 events.
        return Err(DbError::Sqlite(
            "incomplete project mailbox schema; retained project delivery events require delivery and receipt tables to be repaired"
                .to_string(),
        ));
    }
    if has_cursor_state {
        // Retention may remove every shared event. The durable cursor-state
        // catalog entry still proves that shared authority was installed;
        // even an empty or malformed object cannot justify legacy fallback.
        return Err(DbError::Sqlite(
            "incomplete project mailbox schema; durable inbox delivery cursor state requires delivery and receipt tables to be repaired"
                .to_string(),
        ));
    }
    Ok(())
}

/// A join-free absence proof avoids materializing unrelated message history
/// for the common direct-mail-only project. Receipt authority must still be
/// readable: an incomplete schema is never a successfully empty mailbox.
fn has_project_deliveries(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
) -> Result<bool, DbError> {
    let rows = match conn.query_sync(
        "SELECT message_id FROM project_mailbox_deliveries WHERE project_id = ? LIMIT 1",
        &[Value::BigInt(project_id)],
    ) {
        Ok(rows) => rows,
        Err(error) => {
            allow_absent_shared_schema(conn, &error.to_string())?;
            return Ok(false);
        }
    };
    if !rows.is_empty() {
        return Ok(true);
    }
    if let Err(error) = conn.query_sync(
        "SELECT message_id, agent_id, read_ts, ack_ts FROM project_mailbox_receipts LIMIT 0",
        &[],
    ) {
        allow_absent_shared_schema(conn, &error.to_string())?;
    }
    Ok(false)
}

/// Whether a project has shared deliveries, with a consistent schema observation.
///
/// A completely legacy schema is empty; incomplete shared-mail authority is an
/// error so callers cannot mistake an unreadable mailbox for a direct-only one.
pub fn has_project_mailbox_deliveries(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
) -> Result<bool, DbError> {
    crate::sync::inbox::with_snapshot(conn, || has_project_deliveries(conn, project_id))
}

/// Unread messages for every agent in a project, including eligible shared mail.
///
/// Direct and shared receipts are observed together; a direct delivery takes
/// precedence even when its own receipt is already read. Shared history is
/// visited in bounded delivery pages instead of rescanned for every agent.
pub fn project_inbox_unread_counts_from_conn(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
) -> Result<HashMap<i64, i64>, DbError> {
    crate::sync::inbox::with_snapshot(conn, || {
        let mut counts = HashMap::new();
        let rows = conn
            .query_sync(
                "SELECT r.agent_id, COUNT(DISTINCT r.message_id) AS unread \
                 FROM message_recipients r JOIN messages m ON m.id = r.message_id \
                 WHERE m.project_id = ? AND r.read_ts IS NULL GROUP BY r.agent_id",
                &[Value::BigInt(project_id)],
            )
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        merge_unread_counts(&mut counts, rows, None)?;
        if !has_project_deliveries(conn, project_id)? {
            return Ok(counts);
        }
        let mut floor = None;
        let mut skip_at_floor = 0;
        loop {
            let ids = project_mailbox_count_ids(conn, project_id, &mut floor, &mut skip_at_floor)?;
            if ids.is_empty() {
                return Ok(counts);
            }
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!(
                "SELECT viewer.id AS agent_id, COUNT(DISTINCT d.message_id) AS unread \
                 FROM project_mailbox_deliveries d \
                 JOIN messages m ON m.id = d.message_id \
                 JOIN agents viewer ON viewer.project_id = d.project_id \
                 LEFT JOIN project_mailbox_receipts pr \
                     ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
                 WHERE d.project_id = ? AND d.message_id IN ({placeholders}) \
                     AND {VISIBLE_TO_VIEWER_SQL} AND pr.read_ts IS NULL \
                 GROUP BY viewer.id"
            );
            let mut params = vec![Value::BigInt(project_id)];
            params.extend(ids.iter().copied().map(Value::BigInt));
            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            merge_unread_counts(&mut counts, rows, Some(ids.len()))?;
            if ids.len() < 128 {
                return Ok(counts);
            }
        }
    })
}

fn merge_unread_counts(
    counts: &mut HashMap<i64, i64>,
    rows: Vec<sqlmodel_core::Row>,
    maximum: Option<usize>,
) -> Result<(), DbError> {
    for row in rows {
        let agent_id = row
            .get_named::<i64>("agent_id")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let unread = row
            .get_named::<i64>("unread")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        if agent_id <= 0
            || unread < 0
            || maximum.is_some_and(|bound| usize::try_from(unread).map_or(true, |n| n > bound))
        {
            return Err(DbError::Internal(
                "invalid project unread count".to_string(),
            ));
        }
        let total = counts.entry(agent_id).or_default();
        *total = total
            .checked_add(unread)
            .ok_or_else(|| DbError::Internal("project unread count overflow".to_string()))?;
    }
    Ok(())
}

/// Canonical shared-mailbox addresses and their durable to/cc delivery slots
/// for an outbox page. No agent roster or BCC addresses are returned.
pub fn project_mailbox_recipients_from_conn(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    message_ids: &[i64],
) -> Result<HashMap<i64, (String, String)>, DbError> {
    if message_ids.len() > MAX_IN_CLAUSE_ITEMS {
        return Err(DbError::invalid(
            "message_ids",
            "outbox page exceeds 500 messages",
        ));
    }
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    crate::sync::inbox::with_snapshot(conn, || {
        let mut recipients = HashMap::new();
        let placeholders = vec!["?"; message_ids.len()].join(", ");
        let sql = format!(
            "SELECT d.message_id, d.project_id, d.kind, \
                    m.project_id AS message_project_id, p.slug \
             FROM project_mailbox_deliveries d \
             LEFT JOIN messages m ON m.id = d.message_id \
             LEFT JOIN projects p ON p.id = d.project_id \
             WHERE d.message_id IN ({placeholders})"
        );
        let params: Vec<Value> = message_ids.iter().copied().map(Value::BigInt).collect();
        let rows = match conn.query_sync(&sql, &params) {
            Ok(rows) => rows,
            Err(error) => {
                allow_absent_shared_schema(conn, &error.to_string())?;
                return Ok(recipients);
            }
        };
        conn.query_sync(
            "SELECT message_id, agent_id, read_ts, ack_ts FROM project_mailbox_receipts LIMIT 0",
            &[],
        )
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
        for row in rows {
            let id = row
                .get_named::<i64>("message_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let delivery_project = row
                .get_named::<i64>("project_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let message_project = row
                .get_named::<i64>("message_project_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let kind = row
                .get_named::<String>("kind")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let slug = row
                .get_named::<String>("slug")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            if delivery_project != project_id || message_project != project_id {
                return Err(DbError::Internal(
                    "project mailbox delivery identity mismatch".to_string(),
                ));
            }
            if !matches!(kind.as_str(), "to" | "cc") || slug.trim().is_empty() {
                return Err(DbError::Internal(
                    "invalid project mailbox recipient".to_string(),
                ));
            }
            let address = project_mailbox_address(&slug);
            if recipients.insert(id, (address, kind)).is_some() {
                return Err(DbError::Internal(
                    "duplicate project mailbox delivery".to_string(),
                ));
            }
        }
        Ok(recipients)
    })
}

/// An agent's shared-mail obligations, excluding messages delivered directly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProjectMailboxInboxCounts {
    pub unread: usize,
    pub urgent: usize,
    pub ack_required: usize,
    pub ack_overdue: usize,
}

/// Count visible shared mail without loading message bodies.
///
/// All bounded pages share a read snapshot. Read and acknowledgement state are
/// independent, and the canonical visibility predicate gives direct mail
/// precedence over shared delivery of the same message.
pub fn project_mailbox_inbox_counts_from_conn(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    agent_id: i64,
    overdue_before: i64,
) -> Result<ProjectMailboxInboxCounts, DbError> {
    crate::sync::inbox::with_snapshot(conn, || {
        let mut counts = ProjectMailboxInboxCounts::default();
        if !has_project_deliveries(conn, project_id)? {
            return Ok(counts);
        }
        let mut floor = None;
        let mut skip_at_floor = 0;
        loop {
            let ids = project_mailbox_count_ids(conn, project_id, &mut floor, &mut skip_at_floor)?;
            if ids.is_empty() {
                return Ok(counts);
            }
            let page =
                project_mailbox_count_page(conn, project_id, agent_id, overdue_before, &ids)?;
            for (total, increment) in [
                (&mut counts.unread, page.unread),
                (&mut counts.urgent, page.urgent),
                (&mut counts.ack_required, page.ack_required),
                (&mut counts.ack_overdue, page.ack_overdue),
            ] {
                *total = total
                    .checked_add(increment)
                    .ok_or_else(|| DbError::Internal("project inbox count overflow".to_string()))?;
            }
            if ids.len() < 128 {
                return Ok(counts);
            }
        }
    })
}

/// Page in the existing project/timestamp index's order. As in the inbox reader,
/// a single timestamp sort key keeps the pinned engine on its indexed path;
/// the offset counts only rows already consumed at the boundary timestamp.
fn project_mailbox_count_ids(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    floor: &mut Option<i64>,
    skip_at_floor: &mut i64,
) -> Result<Vec<i64>, DbError> {
    let mut sql = "SELECT message_id, delivered_ts FROM project_mailbox_deliveries \
                   WHERE project_id = ?"
        .to_string();
    let mut params = vec![Value::BigInt(project_id)];
    if let Some(timestamp) = *floor {
        sql.push_str(" AND delivered_ts >= ?");
        params.push(Value::BigInt(timestamp));
    }
    sql.push_str(" ORDER BY delivered_ts ASC LIMIT 128 OFFSET ?");
    params.push(Value::BigInt(*skip_at_floor));
    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    if rows.len() > 128 {
        return Err(DbError::Internal(
            "project inbox count page exceeded its bound".to_string(),
        ));
    }
    let mut ids = Vec::with_capacity(rows.len());
    let mut last_ts = *floor;
    let mut trailing_count = *skip_at_floor;
    for row in rows {
        let id = row
            .get_named::<i64>("message_id")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let timestamp = row
            .get_named::<i64>("delivered_ts")
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        if last_ts.is_some_and(|previous| timestamp < previous) {
            return Err(DbError::Internal(
                "project inbox count timestamp moved backwards".to_string(),
            ));
        }
        trailing_count = if last_ts == Some(timestamp) {
            trailing_count.checked_add(1).ok_or_else(|| {
                DbError::Internal("project inbox count timestamp offset overflow".to_string())
            })?
        } else {
            1
        };
        last_ts = Some(timestamp);
        ids.push(id);
    }
    *floor = last_ts;
    *skip_at_floor = trailing_count;
    Ok(ids)
}

fn project_mailbox_count_page(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    agent_id: i64,
    overdue_before: i64,
    ids: &[i64],
) -> Result<ProjectMailboxInboxCounts, DbError> {
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT COUNT(*) AS visible,
                COALESCE(SUM(CASE WHEN pr.read_ts IS NULL THEN 1 ELSE 0 END), 0) AS unread,
                COALESCE(SUM(CASE WHEN pr.read_ts IS NULL
                    AND m.importance IN ('urgent','high') THEN 1 ELSE 0 END), 0) AS urgent,
                COALESCE(SUM(CASE WHEN m.ack_required = 1 AND pr.ack_ts IS NULL
                    THEN 1 ELSE 0 END), 0) AS ack_required,
                COALESCE(SUM(CASE WHEN m.ack_required = 1 AND pr.ack_ts IS NULL
                    AND m.created_ts < ? THEN 1 ELSE 0 END), 0) AS ack_overdue
         FROM project_mailbox_deliveries d
         JOIN messages m ON m.id = d.message_id
         JOIN agents viewer ON viewer.id = ?
         LEFT JOIN project_mailbox_receipts pr
             ON pr.message_id = d.message_id AND pr.agent_id = viewer.id
         WHERE d.project_id = ? AND d.message_id IN ({placeholders})
             AND {VISIBLE_TO_VIEWER_SQL}"
    );
    let mut params = vec![
        Value::BigInt(overdue_before),
        Value::BigInt(agent_id),
        Value::BigInt(project_id),
    ];
    params.extend(ids.iter().copied().map(Value::BigInt));
    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    if rows.len() != 1 {
        return Err(DbError::Internal(
            "project inbox count query did not return one aggregate row".to_string(),
        ));
    }
    let row = &rows[0];
    let count = |name: &str| -> Result<usize, DbError> {
        let raw = row
            .get_named::<i64>(name)
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        let value = usize::try_from(raw)
            .map_err(|_| DbError::Internal(format!("invalid project inbox {name} count")))?;
        if value > ids.len() {
            return Err(DbError::Internal(format!(
                "project inbox {name} count exceeded its page"
            )));
        }
        Ok(value)
    };
    let visible = count("visible")?;
    let bounded_count = |name: &str| -> Result<usize, DbError> {
        let value = count(name)?;
        if value > visible {
            return Err(DbError::Internal(format!(
                "project inbox {name} count exceeded visible messages"
            )));
        }
        Ok(value)
    };
    Ok(ProjectMailboxInboxCounts {
        unread: bounded_count("unread")?,
        urgent: bounded_count("urgent")?,
        ack_required: bounded_count("ack_required")?,
        ack_overdue: bounded_count("ack_overdue")?,
    })
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
    fetch_project_mailbox_rows_with_policy(
        conn, project_id, agent_id, since_ts, limit, options, false,
    )
}

pub(crate) fn fetch_project_mailbox_rows_with_policy(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
    candidates_only: bool,
) -> Result<Vec<InboxRow>, DbError> {
    crate::sync::inbox::with_snapshot(conn, || {
        i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
        if !has_project_deliveries(conn, project_id)? {
            return Ok(Vec::new());
        }
        if let Err(error) = conn.query_sync(
            "SELECT message_id, agent_id, read_ts, ack_ts FROM project_mailbox_receipts LIMIT 0",
            &[],
        ) {
            allow_absent_shared_schema(conn, &error.to_string())?;
        }
        if candidates_only {
            crate::sync::inbox::fetch_shared_with_policy(
                conn, project_id, agent_id, since_ts, limit, options, true,
            )
        } else {
            crate::sync::inbox::fetch_shared(conn, project_id, agent_id, since_ts, limit, options)
        }
    })
}

#[cfg(test)]
fn reference_project_mailbox_rows_from_conn(
    conn: &impl crate::pool::SyncQuery,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxFetchOptions<'_>,
) -> Result<Vec<InboxRow>, DbError> {
    if !has_project_deliveries(conn, project_id)? {
        // Retain the public limit contract even when no delivery is present.
        i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
        return Ok(Vec::new());
    }
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
        Err(error) => {
            allow_absent_shared_schema(conn, &error.to_string())?;
            return Ok(Vec::new());
        }
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
/// These are the `inbox_delivery_events` rows with no recipient agent,
/// in the column shape of the recipient
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
         WHERE e.project_id = ? AND e.agent_id IS NULL AND e.kind = 'project' \
           AND e.seq > ? AND {VISIBLE_TO_VIEWER_SQL} \
         ORDER BY e.seq ASC LIMIT ?"
    )
}

/// Pending shared-mailbox obligations, grouped by delivery for a bounded
/// overview page. Counts represent eligible agents, not stored message copies.
///
/// Uses the inbox visibility predicate so direct recipients take precedence,
/// late joiners and retired agents are excluded, and each agent's own receipt
/// settles only that agent's obligation. The caller pages delivery ids within
/// one read snapshot before this lookup, bounding the joined input even when
/// most historical messages are already settled.
pub fn overview_counts_query(
    message_ids: &[i64],
    overdue_before: i64,
) -> Result<(String, Vec<Value>), DbError> {
    if message_ids.is_empty() || message_ids.len() > MAX_IN_CLAUSE_ITEMS {
        return Err(DbError::invalid(
            "message_ids",
            "overview requires between 1 and 500 delivery ids",
        ));
    }
    let placeholders = std::iter::repeat_n("?", message_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT d.message_id AS id, d.project_id AS project_id, \
                SUM(CASE WHEN pr.read_ts IS NULL THEN 1 ELSE 0 END) AS unread, \
                SUM(CASE WHEN pr.read_ts IS NULL AND m.importance IN ('urgent', 'high') \
                         THEN 1 ELSE 0 END) AS urgent, \
                SUM(CASE WHEN pr.ack_ts IS NULL AND m.ack_required = 1 AND m.created_ts < ? \
                         THEN 1 ELSE 0 END) AS ack_overdue \
         FROM project_mailbox_deliveries d \
         JOIN messages m ON m.id = d.message_id \
         JOIN agents viewer ON viewer.project_id = d.project_id \
         LEFT JOIN project_mailbox_receipts pr \
                ON pr.message_id = d.message_id AND pr.agent_id = viewer.id \
         WHERE d.message_id IN ({placeholders}) AND {VISIBLE_TO_VIEWER_SQL} \
           AND (pr.read_ts IS NULL OR \
                (pr.ack_ts IS NULL AND m.ack_required = 1 AND m.created_ts < ?)) \
         GROUP BY d.message_id, d.project_id ORDER BY d.message_id ASC"
    );
    let mut params = Vec::with_capacity(message_ids.len() + 2);
    params.push(Value::BigInt(overdue_before));
    params.extend(message_ids.iter().copied().map(Value::BigInt));
    params.push(Value::BigInt(overdue_before));
    Ok((sql, params))
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

/// Receipt state is authority, not best-effort display data. A malformed
/// timestamp must never become an apparently unread/unacknowledged delivery.
pub(crate) fn receipt_timestamp(
    row: &sqlmodel_core::Row,
    column: &str,
) -> Result<Option<i64>, DbError> {
    match row.get_by_name(column) {
        Some(Value::Null) => Ok(None),
        Some(Value::BigInt(value)) => Ok(Some(*value)),
        Some(Value::Int(value)) => Ok(Some(i64::from(*value))),
        _ => Err(DbError::Sqlite(format!(
            "receipt column {column} is missing or not an integer"
        ))),
    }
}

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
            Err(error) => {
                allow_absent_shared_schema(conn, &error.to_string())?;
                return Ok(Vec::new());
            }
        };
        for row in rows {
            let message_id = row
                .get_named::<i64>("message_id")
                .map_err(|error| DbError::Sqlite(error.to_string()))?;
            let read_ts = receipt_timestamp(&row, "read_ts")?;
            let ack_ts = receipt_timestamp(&row, "ack_ts")?;
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
/// `(sql, params)` selecting at most `limit` rows with `message_id` and the
/// viewer's existing `ack_ts`, so read-only mutations can verify it is preserved.
#[must_use]
pub fn unread_visible_query(
    agent_id: i64,
    project_id: i64,
    older_than: Option<i64>,
    limit: i64,
) -> (String, Vec<Value>) {
    let mut sql = format!(
        "SELECT d.message_id AS message_id, pr.ack_ts AS ack_ts \
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

/// Own only the transaction this call successfully began. A failed COMMIT can
/// leave it active; a failed body or unwinding must also release its writer.
/// Never roll back a caller's transaction when our nested BEGIN was refused.
struct ReceiptTransaction<'a> {
    conn: &'a DbConn,
    active: bool,
}

impl<'a> ReceiptTransaction<'a> {
    fn begin(conn: &'a DbConn) -> Result<Self, DbError> {
        conn.execute_sync("BEGIN IMMEDIATE", &[])
            .map_err(|error| DbError::Sqlite(error.to_string()))?;
        Ok(Self { conn, active: true })
    }

    fn finish<T>(mut self, result: Result<T, DbError>) -> Result<T, DbError> {
        let result = result.and_then(|value| {
            self.conn
                .execute_sync("COMMIT", &[])
                .map(|_| value)
                .map_err(|error| DbError::Sqlite(error.to_string()))
        });
        match result {
            Ok(value) => {
                self.active = false;
                Ok(value)
            }
            Err(error) => match self.conn.execute_sync("ROLLBACK", &[]) {
                Ok(_) => {
                    self.active = false;
                    Err(error)
                }
                Err(rollback) => {
                    // Drop makes one last cleanup attempt. The caller still
                    // receives an error, not a reusable-session guarantee.
                    Err(DbError::Sqlite(format!(
                        "project mailbox receipt transaction failed ({error}); rollback failed ({rollback})"
                    )))
                }
            },
        }
    }
}

impl Drop for ReceiptTransaction<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.conn.execute_sync("ROLLBACK", &[]);
        }
    }
}

/// Recheck every observed receipt after the writes, within the same transaction.
/// Do not trust affected-row counts: an ignored insert/update is not a receipt.
/// Checking the complete observation also catches a trigger changing a receipt
/// that was already read, or changing an ACK while recording another read.
fn verify_read_receipts(
    conn: &DbConn,
    agent_id: i64,
    message_ids: &[i64],
    expected: &mut [VisibleReceipt],
) -> Result<(), DbError> {
    let mut actual = visible_receipts(conn, agent_id, message_ids)?;
    actual.sort_unstable_by_key(|receipt| receipt.0);
    expected.sort_unstable_by_key(|receipt| receipt.0);
    if actual.as_slice() != &*expected {
        return Err(DbError::Sqlite(
            "project mailbox read receipts did not match the requested state".to_string(),
        ));
    }
    Ok(())
}

/// Record read receipts for the visible, still-unread project deliveries
/// among `message_ids`.
///
/// Ids that are not project deliveries visible to the
/// agent are ignored. Runs in one write transaction on `conn`, verifies the
/// resulting receipts before commit, and rolls back failures. A caller-owned
/// transaction is never committed or rolled back by this function.
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

    let transaction = ReceiptTransaction::begin(conn)?;
    let result = (|| -> Result<Vec<i64>, DbError> {
        let mut updated = Vec::new();
        let mut expected = visible_receipts(conn, agent_id, &unique)?;
        for (message_id, existing_read, _) in &mut expected {
            if existing_read.is_some() {
                continue;
            }
            for (sql, params) in
                receipt_write_statements(ReceiptUpdate::Read, *message_id, agent_id, read_ts)
            {
                conn.execute_sync(sql, &params)
                    .map_err(|error| DbError::Sqlite(error.to_string()))?;
            }
            *existing_read = Some(read_ts);
            updated.push(*message_id);
        }
        if !updated.is_empty() {
            verify_read_receipts(conn, agent_id, &unique, &mut expected)?;
        }
        Ok(updated)
    })();
    let updated = transaction.finish(result)?;
    Ok((!updated.is_empty()).then_some(ProjectMailboxReadBatch {
        read_ts,
        message_ids: updated,
    }))
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
        Err(error) => {
            allow_absent_shared_schema(conn, &error.to_string())?;
            return Ok(None);
        }
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
            read_ts: receipt_timestamp(&row, "read_ts")?,
            ack_ts: receipt_timestamp(&row, "ack_ts")?,
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

    #[test]
    fn project_mailbox_inbox_counts_cross_pages_without_mutating_receipts() {
        let conn = test_conn();
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'counts', '/tmp/counts', 0);
             INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
             VALUES (1, 1, 'Sender', 'test', 'test', 0, 0),
                    (2, 1, 'Reader', 'test', 'test', 0, 0);
             BEGIN;",
        )
        .unwrap();
        for message_id in 1_i64..=257 {
            let importance = if message_id % 2 == 0 {
                "high"
            } else {
                "normal"
            };
            let ack_required = i64::from(message_id % 3 == 0);
            let created_ts = message_id * 1_000;
            // A timestamp tie spans the 128-row page boundary.
            let delivered_ts = message_id / 200;
            conn.execute_raw(&format!(
                "INSERT INTO messages
                 (id, project_id, sender_id, subject, body_md, created_ts, importance, ack_required)
                 VALUES ({message_id}, 1, 1, 'shared', '', {created_ts}, '{importance}', {ack_required});
                 INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts)
                 VALUES ({message_id}, 1, 'to', {delivered_ts});"
            ))
            .unwrap();
        }
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
             VALUES (129, 2, 'to', 300000, 300000);
             INSERT INTO project_mailbox_receipts (message_id, agent_id, read_ts, ack_ts)
             VALUES (130, 2, 300000, 300000),
                    (132, 2, 300000, NULL),
                    (135, 2, NULL, 300000);
             COMMIT;
             PRAGMA query_only = ON;",
        )
        .unwrap();
        let counts = project_mailbox_inbox_counts_from_conn(&conn, 1, 2, 150_000).unwrap();
        assert_eq!(
            counts,
            ProjectMailboxInboxCounts {
                unread: 254,
                urgent: 126,
                ack_required: 83,
                ack_overdue: 47,
            }
        );
        assert_eq!(
            project_mailbox_inbox_counts_from_conn(&conn, 1, 99, 150_000).unwrap(),
            ProjectMailboxInboxCounts::default()
        );
        assert_eq!(
            project_inbox_unread_counts_from_conn(&conn, 1).unwrap(),
            HashMap::from([(2, 254)])
        );
        let receipts = conn
            .query_sync("SELECT COUNT(*) AS n FROM project_mailbox_receipts", &[])
            .unwrap();
        assert_eq!(receipts[0].get_named::<i64>("n").unwrap(), 3);
        let activity = conn
            .query_sync("SELECT SUM(last_active_ts) AS n FROM agents", &[])
            .unwrap();
        assert_eq!(activity[0].get_named::<i64>("n").unwrap(), 0);
    }

    fn resource_delivery_fixture() -> DbConn {
        let conn = test_conn();
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'backend', '/backend', 0), (2, 'other', '/other', 0);
             INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
             VALUES (1, 1, 'Sender', 'test', 'test', 0, 0),
                    (2, 1, 'Reader', 'test', 'test', 0, 0),
                    (3, 1, 'Peer', 'test', 'test', 0, 0),
                    (4, 1, 'Blocked', 'test', 'test', 0, 0),
                    (5, 1, 'Retired', 'test', 'test', 0, 0),
                    (6, 1, 'Late', 'test', 'test', 2000, 0),
                    (7, 2, 'Foreign', 'test', 'test', 0, 0),
                    (8, 1, 'Hidden', 'test', 'test', 0, 0);
             UPDATE agents SET contact_policy = 'BLOCK_ALL' WHERE id = 4;
             UPDATE agents SET retired_at = 2000 WHERE id = 5;
             INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts, importance)
             VALUES (100, 1, 1, 'Shared ordinary', '', 100, 'normal'),
                    (101, 1, 1, 'Mixed urgent', '', 200, 'urgent'),
                    (102, 1, 1, 'Direct ordinary', '', 300, 'normal');
             INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts)
             VALUES (100, 1, 'to', 100), (101, 1, 'cc', 200);
             INSERT INTO message_recipients (message_id, agent_id, kind, read_ts)
             VALUES (101, 2, 'to', 400), (101, 8, 'bcc', NULL), (102, 2, 'to', NULL);
             INSERT INTO project_mailbox_receipts (message_id, agent_id, read_ts)
             VALUES (100, 2, 400);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn resource_delivery_helpers_preserve_visibility_receipts_and_slots() {
        let conn = resource_delivery_fixture();
        conn.execute_raw("PRAGMA query_only = ON").unwrap();
        assert_eq!(
            project_inbox_unread_counts_from_conn(&conn, 1).unwrap(),
            HashMap::from([(2, 1), (3, 2), (8, 2)])
        );
        assert!(
            project_inbox_unread_counts_from_conn(&conn, 2)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            project_mailbox_recipients_from_conn(&conn, 1, &[100, 101, 102]).unwrap(),
            HashMap::from([
                (100, ("project:backend".to_string(), "to".to_string())),
                (101, ("project:backend".to_string(), "cc".to_string())),
            ])
        );
        assert!(project_mailbox_recipients_from_conn(&conn, 2, &[100]).is_err());
        assert!(project_mailbox_recipients_from_conn(&conn, 1, &[100; 501]).is_err());
        let receipts = conn
            .query_sync("SELECT COUNT(*) AS n FROM project_mailbox_receipts", &[])
            .unwrap();
        assert_eq!(receipts[0].get_named::<i64>("n").unwrap(), 1);
        let direct = conn
            .query_sync("SELECT COUNT(*) AS n FROM message_recipients", &[])
            .unwrap();
        assert_eq!(direct[0].get_named::<i64>("n").unwrap(), 3);
        let activity = conn
            .query_sync("SELECT SUM(last_active_ts) AS n FROM agents", &[])
            .unwrap();
        assert_eq!(activity[0].get_named::<i64>("n").unwrap(), 0);
    }

    #[test]
    fn outbox_project_delivery_rejects_invalid_kind_and_identity() {
        for mutation in [
            "UPDATE project_mailbox_deliveries SET kind = 'bcc' WHERE message_id = 100",
            "UPDATE project_mailbox_deliveries SET project_id = 2 WHERE message_id = 100",
            "PRAGMA foreign_keys = OFF; UPDATE project_mailbox_deliveries SET project_id = 999 WHERE message_id = 100",
            "PRAGMA foreign_keys = OFF; UPDATE project_mailbox_deliveries SET message_id = 999 WHERE message_id = 100",
            "UPDATE projects SET slug = '' WHERE id = 1",
        ] {
            let conn = resource_delivery_fixture();
            conn.execute_raw(mutation).unwrap();
            assert!(
                project_mailbox_recipients_from_conn(&conn, 1, &[100, 999]).is_err(),
                "{mutation}"
            );
        }
    }

    #[test]
    fn resource_delivery_helpers_reject_incomplete_or_broken_shared_schema() {
        for mutation in [
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts",
            "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries",
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts;
             ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries",
            "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries;
             CREATE VIEW project_mailbox_deliveries AS SELECT * FROM missing_delivery_source",
        ] {
            let conn = resource_delivery_fixture();
            conn.execute_raw(mutation).unwrap();
            assert!(
                project_inbox_unread_counts_from_conn(&conn, 1).is_err(),
                "{mutation}"
            );
            assert!(
                project_mailbox_recipients_from_conn(&conn, 1, &[100]).is_err(),
                "{mutation}"
            );
        }
    }

    fn receipt_fixture(test: impl FnOnce(&DbConn)) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.sqlite3");
        let conn = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "project mailbox receipt fixture",
        );
        for sql in [
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE projects (id INTEGER PRIMARY KEY)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), inception_ts INTEGER NOT NULL, retired_at INTEGER, contact_policy TEXT)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id), sender_id INTEGER NOT NULL REFERENCES agents(id), created_ts INTEGER NOT NULL)",
            "CREATE TABLE message_recipients (message_id INTEGER REFERENCES messages(id), agent_id INTEGER REFERENCES agents(id), PRIMARY KEY(message_id, agent_id))",
            "CREATE TABLE project_mailbox_deliveries (message_id INTEGER PRIMARY KEY REFERENCES messages(id), project_id INTEGER NOT NULL REFERENCES projects(id))",
            "CREATE TABLE project_mailbox_receipts (message_id INTEGER REFERENCES messages(id), agent_id INTEGER REFERENCES agents(id), read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            "INSERT INTO projects VALUES (1)",
            "INSERT INTO agents VALUES (10, 1, 1, NULL, 'auto'), (20, 1, 1, NULL, 'auto'), (30, 1, 1, NULL, 'auto')",
            "INSERT INTO messages VALUES (100, 1, 10, 1000), (200, 1, 10, 2000)",
            "INSERT INTO project_mailbox_deliveries VALUES (100, 1), (200, 1)",
        ] {
            conn.execute_raw(sql).unwrap();
        }
        test(&conn);
    }

    fn assert_receipt_writer_released(conn: &DbConn) {
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        conn.execute_raw("ROLLBACK").unwrap();
    }

    #[test]
    fn read_batch_verifies_receipts_and_preserves_existing_ack_and_peer_state() {
        receipt_fixture(|conn| {
            conn.execute_raw(
                "INSERT INTO project_mailbox_receipts VALUES (100, 20, NULL, 77), (200, 20, 55, 66), (100, 30, 11, 22)",
            )
            .unwrap();
            let first = mark_project_mailbox_read_batch_sync_conn(conn, 20, &[200, 100, 100, 999])
                .unwrap()
                .unwrap();
            assert_eq!(first.message_ids, vec![100]);
            let mut receipts = visible_receipts(conn, 20, &[100, 200]).unwrap();
            receipts.sort_unstable_by_key(|receipt| receipt.0);
            assert_eq!(
                receipts,
                vec![
                    (100, Some(first.read_ts), Some(77)),
                    (200, Some(55), Some(66))
                ]
            );
            assert_eq!(
                visible_receipts(conn, 30, &[100]).unwrap(),
                vec![(100, Some(11), Some(22))]
            );
            assert_eq!(
                mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200]).unwrap(),
                None
            );
            assert!(
                conn.query_sync("SELECT 1 FROM message_recipients", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_receipt_writer_released(conn);
        });
    }

    #[test]
    fn ignored_receipt_insert_or_update_refuses_success_and_rolls_back_the_batch() {
        for operation in ["INSERT", "UPDATE"] {
            receipt_fixture(|conn| {
                conn.execute_raw(&format!(
                    "CREATE TRIGGER ignore_receipt BEFORE {operation} ON project_mailbox_receipts \
                     WHEN NEW.message_id = 200 BEGIN SELECT RAISE(IGNORE); END"
                ))
                .unwrap();
                let error = mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200])
                    .expect_err("an ignored write is not a read receipt");
                assert!(error.to_string().contains("did not match"));
                assert!(
                    conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                        .unwrap()
                        .is_empty()
                );
                assert_receipt_writer_released(conn);
            });
        }
    }

    #[test]
    fn trigger_changed_ack_is_not_committed_as_a_read_receipt() {
        receipt_fixture(|conn| {
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, NULL, 77)")
                .unwrap();
            conn.execute_raw(
                "CREATE TRIGGER change_ack AFTER UPDATE OF read_ts ON project_mailbox_receipts \
                 BEGIN UPDATE project_mailbox_receipts SET ack_ts = 99 \
                       WHERE message_id = NEW.message_id AND agent_id = NEW.agent_id; END",
            )
            .unwrap();
            assert!(mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100]).is_err());
            assert_eq!(
                visible_receipts(conn, 20, &[100]).unwrap(),
                vec![(100, None, Some(77))]
            );
            assert_receipt_writer_released(conn);
        });
    }

    #[test]
    fn malformed_receipt_state_is_not_treated_as_an_unread_delivery() {
        for column in ["read_ts", "ack_ts"] {
            receipt_fixture(|conn| {
                conn.execute_raw(&format!(
                    "INSERT INTO project_mailbox_receipts (message_id, agent_id, {column}) VALUES (100, 20, X'01')"
                ))
                .unwrap();
                let error = mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200])
                    .expect_err("malformed receipt must be refused");
                assert!(error.to_string().contains("not an integer"));
                let rows = conn
                    .query_sync("SELECT message_id FROM project_mailbox_receipts", &[])
                    .unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].get_named::<i64>("message_id").unwrap(), 100);
                assert_receipt_writer_released(conn);
            });
        }
    }

    #[test]
    fn receipt_transaction_rolls_back_body_errors_and_unwinding() {
        receipt_fixture(|conn| {
            let transaction = ReceiptTransaction::begin(conn).unwrap();
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, 42, NULL)")
                .unwrap();
            let result = conn
                .execute_raw("INSERT INTO missing_receipt_table VALUES (1)")
                .map_err(|error| DbError::Sqlite(error.to_string()));
            assert!(transaction.finish(result).is_err());
            assert!(
                conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_receipt_writer_released(conn);

            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _transaction = ReceiptTransaction::begin(conn).unwrap();
                conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, 42, NULL)")
                    .unwrap();
                panic!("receipt transaction unwind");
            }));
            assert!(panicked.is_err());
            assert!(
                conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_receipt_writer_released(conn);
        });
    }

    #[test]
    fn failed_receipt_commit_releases_transaction_without_publishing_reads() {
        receipt_fixture(|conn| {
            conn.execute_raw(
                "CREATE TABLE deferred_receipt_check (agent_id INTEGER REFERENCES agents(id) DEFERRABLE INITIALLY DEFERRED)",
            )
            .unwrap();
            // This is a real COMMIT failure, not an early statement error.
            conn.execute_raw("BEGIN IMMEDIATE").unwrap();
            conn.execute_raw("INSERT INTO deferred_receipt_check VALUES (999)")
                .unwrap();
            assert!(conn.execute_raw("COMMIT").is_err());
            conn.execute_raw("ROLLBACK").unwrap();

            conn.execute_raw(
                "CREATE TRIGGER fail_receipt_commit AFTER INSERT ON project_mailbox_receipts \
                 BEGIN INSERT INTO deferred_receipt_check VALUES (999); END",
            )
            .unwrap();
            assert!(mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200]).is_err());
            assert!(
                conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                    .unwrap()
                    .is_empty()
            );
            assert!(
                conn.query_sync("SELECT 1 FROM deferred_receipt_check", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_receipt_writer_released(conn);
        });
    }

    #[test]
    fn nested_receipt_call_preserves_the_callers_transaction() {
        receipt_fixture(|conn| {
            conn.execute_raw("BEGIN IMMEDIATE").unwrap();
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 30, 11, 22)")
                .unwrap();
            assert!(mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100]).is_err());
            assert_eq!(
                visible_receipts(conn, 30, &[100]).unwrap(),
                vec![(100, Some(11), Some(22))]
            );
            conn.execute_raw("ROLLBACK").unwrap();
            assert!(
                conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_receipt_writer_released(conn);
        });
    }

    fn add_reader_columns(conn: &DbConn) {
        for addition in [
            "agents ADD COLUMN name TEXT NOT NULL DEFAULT 'Viewer'",
            "messages ADD COLUMN thread_id TEXT",
            "messages ADD COLUMN topic TEXT",
            "messages ADD COLUMN subject TEXT NOT NULL DEFAULT 'shared'",
            "messages ADD COLUMN body_md TEXT NOT NULL DEFAULT 'body'",
            "messages ADD COLUMN importance TEXT NOT NULL DEFAULT 'normal'",
            "messages ADD COLUMN ack_required INTEGER NOT NULL DEFAULT 1",
            "messages ADD COLUMN recipients_json TEXT NOT NULL DEFAULT '{}'",
            "messages ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]'",
            "project_mailbox_deliveries ADD COLUMN kind TEXT NOT NULL DEFAULT 'to'",
            "project_mailbox_deliveries ADD COLUMN delivered_ts INTEGER NOT NULL DEFAULT 1000",
        ] {
            conn.execute_raw(&format!("ALTER TABLE {addition}"))
                .unwrap();
        }
    }

    #[test]
    fn missing_receipt_errors_are_not_classified_as_empty_shared_deliveries() {
        for error in [
            "no such table: project_mailbox_receipts",
            "Query error: no such table: main.project_mailbox_receipts",
            "no such table: project_mailbox_deliveries_backup",
            "no such table: messages; SELECT * FROM project_mailbox_deliveries",
            "database is busy: project_mailbox_deliveries",
        ] {
            assert!(!is_missing_project_mailbox_table_error(error), "{error}");
        }
        for error in [
            "no such table: project_mailbox_deliveries",
            "Query error: no such table: main.project_mailbox_deliveries",
            "no such table: \"PROJECT_MAILBOX_DELIVERIES\"",
        ] {
            assert!(is_missing_project_mailbox_table_error(error), "{error}");
        }
    }

    #[test]
    fn missing_receipt_table_refuses_reads_and_writeback_until_schema_is_restored() {
        receipt_fixture(|conn| {
            add_reader_columns(conn);
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, 42, 77)")
                .unwrap();
            assert_eq!(visible_ids(conn, 1, 20), vec![200, 100]);
            conn.execute_raw("ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts")
                .unwrap();
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            let error = fetch_project_mailbox_rows_from_conn(conn, 1, 20, None, 10, options())
                .expect_err("missing receipts cannot erase an existing shared inbox");
            assert!(
                error
                    .to_string()
                    .contains("incomplete project mailbox schema")
            );
            assert!(!is_missing_project_mailbox_table_error(&error.to_string()));
            assert!(project_mailbox_delivery_from_conn(conn, 100).is_err());
            conn.execute_raw("PRAGMA query_only = OFF").unwrap();
            assert!(mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200]).is_err());
            assert_receipt_writer_released(conn);

            conn.execute_raw("ALTER TABLE retained_receipts RENAME TO project_mailbox_receipts")
                .unwrap();
            assert_eq!(visible_ids(conn, 1, 20), vec![200, 100]);
            assert_eq!(
                visible_receipts(conn, 20, &[100]).unwrap(),
                vec![(100, Some(42), Some(77))]
            );
            assert_eq!(
                mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100, 200])
                    .unwrap()
                    .unwrap()
                    .message_ids,
                vec![200]
            );
        });
    }

    #[test]
    fn missing_delivery_table_with_retained_receipts_is_not_a_legacy_empty_schema() {
        receipt_fixture(|conn| {
            add_reader_columns(conn);
            conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, 42, 77)")
                .unwrap();
            conn.execute_raw(
                "ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries",
            )
            .unwrap();
            for error in [
                fetch_project_mailbox_rows_from_conn(conn, 1, 20, None, 10, options())
                    .expect_err("missing delivery authority"),
                project_mailbox_delivery_from_conn(conn, 100)
                    .expect_err("orphan receipts are not a pre-upgrade schema"),
                mark_project_mailbox_read_batch_sync_conn(conn, 20, &[100])
                    .expect_err("must not acknowledge missing delivery authority"),
            ] {
                assert!(
                    error
                        .to_string()
                        .contains("incomplete project mailbox schema")
                );
                assert!(!is_missing_project_mailbox_table_error(&error.to_string()));
            }
            assert_receipt_writer_released(conn);
            assert_eq!(
                conn.query_sync("SELECT ack_ts FROM project_mailbox_receipts", &[])
                    .unwrap()[0]
                    .get_named::<i64>("ack_ts")
                    .unwrap(),
                77
            );
        });
    }

    #[test]
    fn surviving_shared_events_refuse_missing_tables_across_read_and_receipt_surfaces() {
        let conn = crate::guard_db_conn(test_conn(), "shared schema loss regression");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'direct', '/direct', 0), (2, 'shared', '/shared', 0);
             INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
             VALUES (10, 1, 'DirectSender', 'test', 'test', 0, 0),
                    (20, 1, 'DirectReader', 'test', 'test', 0, 0),
                    (30, 2, 'SharedSender', 'test', 'test', 0, 0),
                    (40, 2, 'SharedReader', 'test', 'test', 0, 0);
             INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
             VALUES (100, 1, 10, 'Direct message', 'direct body', 100),
                    (200, 2, 30, 'Shared message', 'shared body', 200);
             INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (100, 20, 'to');
             INSERT INTO project_mailbox_deliveries (message_id, project_id, kind, delivered_ts)
             VALUES (200, 2, 'to', 200);
             INSERT INTO project_mailbox_receipts (message_id, agent_id, read_ts, ack_ts)
             VALUES (200, 40, 300, 400);",
        )
        .unwrap();
        conn.execute_sync(
            INSERT_PROJECT_MAILBOX_EVENT_SQL,
            &[Value::BigInt(2), Value::BigInt(200), Value::BigInt(200)],
        )
        .unwrap();
        assert_eq!(visible_ids(&conn, 2, 40), vec![200]);
        conn.execute_raw(
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts;
             ALTER TABLE project_mailbox_deliveries RENAME TO retained_deliveries;
             PRAGMA query_only = ON;",
        )
        .unwrap();

        // A project event anywhere proves these global tables were installed.
        // Even a direct-only project must not receive a partial-success claim.
        let error =
            crate::sync::fetch_inbox_filtered_rows_from_conn(&conn, 1, 20, None, 10, options())
                .expect_err("direct rows do not authorize a partial inbox");
        assert!(
            error
                .to_string()
                .contains("retained project delivery events")
        );
        let error = fetch_project_mailbox_rows_from_conn(&*conn, 2, 40, None, 10, options())
            .expect_err("shared messages must not disappear as an empty inbox");
        assert!(
            error
                .to_string()
                .contains("retained project delivery events")
        );
        let error = project_mailbox_delivery_from_conn(&*conn, 200)
            .expect_err("delivery reports must not erase the shared address");
        assert!(
            error
                .to_string()
                .contains("retained project delivery events")
        );
        assert!(!is_missing_project_mailbox_table_error(&error.to_string()));
        let error = crate::sync::inbox_delivery_events_from_conn(&*conn, 1, 20, None, 10)
            .expect_err("the global schema witness also protects delivery cursors");
        assert!(
            error
                .to_string()
                .contains("retained project delivery events")
        );
        conn.execute_raw("PRAGMA query_only = OFF").unwrap();
        let error = mark_project_mailbox_read_batch_sync_conn(&conn, 40, &[200])
            .expect_err("a missing authority cannot become a successful no-op receipt");
        assert!(
            error
                .to_string()
                .contains("retained project delivery events")
        );
        assert_receipt_writer_released(&conn);

        conn.execute_raw(
            "ALTER TABLE retained_deliveries RENAME TO project_mailbox_deliveries;
             ALTER TABLE retained_receipts RENAME TO project_mailbox_receipts;",
        )
        .unwrap();
        assert_eq!(visible_ids(&conn, 2, 40), vec![200]);
        assert_eq!(
            visible_receipts(&*conn, 40, &[200]).unwrap(),
            vec![(200, Some(300), Some(400))],
            "failed reads and receipt attempts retain the original state"
        );
        let direct =
            crate::sync::fetch_inbox_filtered_rows_from_conn(&conn, 1, 20, None, 10, options())
                .unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].message.id, Some(100));
    }

    #[test]
    fn direct_only_event_history_still_allows_a_legacy_shared_schema() {
        let conn = crate::guard_db_conn(test_conn(), "legacy shared schema regression");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'legacy', '/legacy', 0);
             INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
             VALUES (10, 1, 'Sender', 'test', 'test', 0, 0), (20, 1, 'Reader', 'test', 'test', 0, 0);
             INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
             VALUES (100, 1, 10, 'Direct legacy message', 'body', 100);
             INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (100, 20, 'to');
             INSERT OR IGNORE INTO inbox_delivery_events (project_id, agent_id, message_id, kind, delivered_ts)
             VALUES (1, 20, 100, 'to', 100);
             ALTER TABLE project_mailbox_receipts RENAME TO retained_empty_receipts;
             ALTER TABLE project_mailbox_deliveries RENAME TO retained_empty_deliveries;
             ALTER TABLE inbox_delivery_cursor_state RENAME TO retained_cursor_state;
             PRAGMA query_only = ON;",
        )
        .unwrap();
        let rows =
            crate::sync::fetch_inbox_filtered_rows_from_conn(&conn, 1, 20, None, 10, options())
                .expect("direct-only event history is not evidence of shared mail");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message.id, Some(100));
        assert!(
            fetch_project_mailbox_rows_from_conn(&*conn, 1, 20, None, 10, options())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            project_mailbox_delivery_from_conn(&*conn, 100).unwrap(),
            None
        );
        assert_eq!(
            project_inbox_unread_counts_from_conn(&*conn, 1).unwrap(),
            HashMap::from([(20, 1)])
        );
        assert!(
            project_mailbox_recipients_from_conn(&*conn, 1, &[100])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn durable_cursor_schema_refuses_legacy_fallback_after_all_events_are_pruned() {
        for object in [
            "CREATE TABLE inbox_delivery_cursor_state (tail_seq INTEGER)",
            "CREATE VIEW inbox_delivery_cursor_state AS SELECT 0 AS tail_seq",
        ] {
            let conn = crate::guard_db_conn(
                DbConn::open_memory().unwrap(),
                "durable cursor schema authority regression",
            );
            let missing = "no such table: project_mailbox_deliveries";
            allow_absent_shared_schema(&*conn, missing)
                .expect("a genuinely pre-upgrade catalog can use the legacy fallback");
            conn.execute_raw(object).unwrap();
            conn.execute_raw("PRAGMA query_only = ON").unwrap();
            let error = allow_absent_shared_schema(&*conn, missing)
                .expect_err("cursor authority survives even completely pruned event history");
            assert!(
                error
                    .to_string()
                    .contains("durable inbox delivery cursor state")
            );
            assert!(!is_missing_project_mailbox_table_error(&error.to_string()));
        }
    }

    #[test]
    fn legacy_schema_fallback_requires_a_readable_ledger_and_explicit_project_marker() {
        let conn = crate::guard_db_conn(
            DbConn::open_memory().unwrap(),
            "legacy delivery marker regression",
        );
        conn.execute_raw(
            "CREATE TABLE inbox_delivery_events (seq INTEGER PRIMARY KEY, agent_id INTEGER, kind TEXT);
             INSERT INTO inbox_delivery_events VALUES (1, NULL, 'to');",
        )
        .unwrap();
        assert!(
            fetch_project_mailbox_rows_from_conn(&*conn, 1, 1, None, 10, options())
                .unwrap()
                .is_empty()
        );
        for agent_id in [Some(0_i64), None] {
            conn.execute_sync(
                "UPDATE inbox_delivery_events SET agent_id = ?, kind = 'project'",
                &[agent_id.map_or(Value::Null, Value::BigInt)],
            )
            .unwrap();
            assert!(
                fetch_project_mailbox_rows_from_conn(&*conn, 1, 1, None, 10, options())
                    .unwrap_err()
                    .to_string()
                    .contains("retained project delivery events")
            );
        }
        // The event catalog entry is present but its contents cannot be
        // inspected. This is unavailable evidence, never an empty mailbox.
        conn.execute_raw(
            "ALTER TABLE inbox_delivery_events RENAME TO retained_events;
             CREATE TABLE inbox_delivery_events (seq INTEGER PRIMARY KEY);",
        )
        .unwrap();
        assert!(fetch_project_mailbox_rows_from_conn(&*conn, 1, 1, None, 10, options()).is_err());
    }

    #[test]
    fn delivery_receipt_report_refuses_malformed_state_without_overwriting_it() {
        for column in ["read_ts", "ack_ts"] {
            receipt_fixture(|conn| {
                add_reader_columns(conn);
                conn.execute_raw("INSERT INTO project_mailbox_receipts VALUES (100, 20, 42, 77)")
                    .unwrap();
                let receipt = project_mailbox_delivery_from_conn(conn, 100)
                    .unwrap()
                    .unwrap();
                let reader = receipt
                    .agents
                    .iter()
                    .find(|agent| agent.agent_id == 20)
                    .unwrap();
                assert_eq!((reader.read_ts, reader.ack_ts), (Some(42), Some(77)));
                conn.execute_raw(&format!(
                    "UPDATE project_mailbox_receipts SET {column} = X'0102' WHERE agent_id = 20"
                ))
                .unwrap();
                let error = project_mailbox_delivery_from_conn(conn, 100)
                    .expect_err("malformed state must not be reported as NULL");
                assert!(error.to_string().contains("not an integer"));
                let rows = conn
                    .query_sync(
                        &format!(
                            "SELECT typeof({column}) AS storage_type FROM project_mailbox_receipts"
                        ),
                        &[],
                    )
                    .unwrap();
                assert_eq!(rows[0].get_named::<String>("storage_type").unwrap(), "blob");
            });
        }
    }

    #[test]
    fn receipt_verification_covers_the_last_sql_chunk_and_preserves_atomicity() {
        for ignore_last in [false, true] {
            receipt_fixture(|conn| {
                let last = 1000 + i64::try_from(MAX_IN_CLAUSE_ITEMS).unwrap();
                conn.execute_raw("BEGIN IMMEDIATE").unwrap();
                for message in 1000..=last {
                    conn.execute_sync(
                        "INSERT INTO messages VALUES (?, 1, 10, 3000)",
                        &[Value::BigInt(message)],
                    )
                    .unwrap();
                    conn.execute_sync(
                        "INSERT INTO project_mailbox_deliveries VALUES (?, 1)",
                        &[Value::BigInt(message)],
                    )
                    .unwrap();
                }
                conn.execute_raw("COMMIT").unwrap();
                if ignore_last {
                    conn.execute_raw(&format!(
                        "CREATE TRIGGER ignore_last BEFORE UPDATE ON project_mailbox_receipts \
                         WHEN NEW.message_id = {last} BEGIN SELECT RAISE(IGNORE); END"
                    ))
                    .unwrap();
                }
                let ids: Vec<i64> = (1000..=last).collect();
                let result = mark_project_mailbox_read_batch_sync_conn(conn, 20, &ids);
                if ignore_last {
                    assert!(result.is_err());
                    assert!(
                        conn.query_sync("SELECT 1 FROM project_mailbox_receipts", &[])
                            .unwrap()
                            .is_empty()
                    );
                } else {
                    assert_eq!(result.unwrap().unwrap().message_ids.len(), ids.len());
                    assert!(
                        mark_project_mailbox_read_batch_sync_conn(conn, 20, &ids)
                            .unwrap()
                            .is_none()
                    );
                }
                assert_receipt_writer_released(conn);
            });
        }
    }

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
    fn empty_shared_project_uses_only_bounded_delivery_and_receipt_probes() {
        struct Observed<'a> {
            conn: &'a DbConn,
            statements: std::cell::RefCell<Vec<String>>,
        }
        impl crate::pool::SyncQuery for Observed<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                self.statements.borrow_mut().push(sql.to_string());
                self.conn.query_sync(sql, params)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.conn.execute_raw(sql)
            }
        }
        let conn = test_conn();
        let observed = Observed {
            conn: &conn,
            statements: std::cell::RefCell::new(Vec::new()),
        };
        assert!(
            fetch_project_mailbox_rows_from_conn(&observed, 1, 2, None, 20, options())
                .unwrap()
                .is_empty()
        );
        let statements = observed.statements.borrow();
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("WHERE project_id = ? LIMIT 1"));
        assert!(statements[1].contains("project_mailbox_receipts LIMIT 0"));
        assert!(statements.iter().all(|sql| !sql.contains("JOIN")));
        drop(statements);
        #[cfg(target_pointer_width = "64")]
        assert!(
            fetch_project_mailbox_rows_from_conn(&observed, 1, 2, None, usize::MAX, options())
                .is_err()
        );
    }

    #[test]
    fn empty_delivery_probe_still_refuses_missing_receipt_authority() {
        let conn = test_conn();
        conn.execute_raw(
            "ALTER TABLE project_mailbox_receipts RENAME TO retained_project_receipts",
        )
        .unwrap();
        let error =
            fetch_project_mailbox_rows_from_conn(&conn, 1, 2, None, 20, options()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incomplete project mailbox schema")
        );
        assert_eq!(
            conn.query_sync(
                "SELECT name FROM sqlite_master WHERE name = 'retained_project_receipts'",
                &[]
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn paged_shared_inbox_matches_join_for_every_filter_and_body_policy() {
        let conn = shared_reader_fixture(32);
        for flags in 0..8 {
            for since in [None, Some(16_000)] {
                for overdue in [None, Some(24_000)] {
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
                                let expected = reference_project_mailbox_rows_from_conn(
                                    &conn, 1, 2, since, limit, filters,
                                )
                                .unwrap();
                                let actual = fetch_project_mailbox_rows_from_conn(
                                    &conn, 1, 2, since, limit, filters,
                                )
                                .unwrap();
                                assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
                            }
                        }
                    }
                }
            }
        }
    }

    fn shared_reader_fixture(count: i64) -> DbConn {
        let conn = test_conn();
        conn.execute_raw(
            "INSERT INTO projects(id,slug,human_key,created_at) VALUES
            (1,'shared-one','/shared-one',0),(2,'shared-two','/shared-two',0);
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES
            (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',0,0),
            (3,1,'RedFox','test','test',0,0)",
        )
        .unwrap();
        seed_shared_reader(&conn, 1, count);
        conn
    }

    fn seed_shared_reader(conn: &DbConn, first: i64, last: i64) {
        conn.execute_raw("BEGIN").unwrap();
        for start in (first..=last).step_by(128) {
            let end = (start + 127).min(last);
            let messages = (start..=end)
                .map(|id| {
                    format!(
                        "({id},1,{},'subject-{id}','body-{id}',{},'{}',{},'{}')",
                        if id % 5 == 0 { 2 } else { 1 },
                        id * 1000,
                        ["normal", "high", "urgent", "low"][usize::try_from(id % 4).unwrap()],
                        i64::from(id % 3 != 0),
                        ["Handoffs", "other", "é", "É"][usize::try_from(id % 4).unwrap()]
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,
                created_ts,importance,ack_required,topic) VALUES {messages}"
            ))
            .unwrap();
            // Deliberately different delivery timestamps: message creation is
            // the inbox ordering authority, including reconstructed mailboxes.
            let deliveries = (start..=end)
                .map(|id| format!("({id},1,'to',{})", 1_000_000 - id))
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO project_mailbox_deliveries
                (message_id,project_id,kind,delivered_ts) VALUES {deliveries}"
            ))
            .unwrap();
            let receipts = (start..=end)
                .map(|id| {
                    format!(
                        "({id},2,{},{})",
                        if id % 3 == 0 { "2000" } else { "NULL" },
                        if id % 4 == 0 { "3000" } else { "NULL" }
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO project_mailbox_receipts
                (message_id,agent_id,read_ts,ack_ts) VALUES {receipts}"
            ))
            .unwrap();
            let direct = (start..=end)
                .filter(|id| id % 7 == 0)
                .map(|id| format!("({id},2,'bcc')"))
                .collect::<Vec<_>>()
                .join(",");
            if !direct.is_empty() {
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients(message_id,agent_id,kind)
                    VALUES {direct}"
                ))
                .unwrap();
            }
        }
        conn.execute_raw("COMMIT").unwrap();
    }

    #[test]
    fn shared_inbox_finishes_large_timestamp_ties_and_skips_invisible_pages() {
        let conn = shared_reader_fixture(600);
        conn.execute_raw(
            "UPDATE messages SET created_ts=1000;
            UPDATE project_mailbox_deliveries SET project_id=2 WHERE message_id=599;
            UPDATE messages SET project_id=2 WHERE id=598;
            UPDATE project_mailbox_receipts SET read_ts=1 WHERE message_id>300;
            UPDATE agents SET inception_ts=1000 WHERE id=2",
        )
        .unwrap();
        for filters in [
            options(),
            InboxFetchOptions {
                unread_only: true,
                ..options()
            },
        ] {
            let expected =
                reference_project_mailbox_rows_from_conn(&conn, 1, 2, None, 50, filters).unwrap();
            let actual =
                fetch_project_mailbox_rows_from_conn(&conn, 1, 2, None, 50, filters).unwrap();
            assert_eq!(actual.len(), 50);
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        }
    }

    #[test]
    fn shared_inbox_pages_ten_thousand_deliveries_without_materializing_history() {
        struct Observed<'a> {
            conn: &'a DbConn,
            calls: std::cell::RefCell<Vec<(String, Vec<Value>, usize)>>,
        }
        impl crate::pool::SyncQuery for Observed<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.conn.query_sync(sql, params)?;
                if sql.contains("FROM project_mailbox_receipts WHERE message_id IN")
                    || sql.contains("FROM message_recipients WHERE message_id IN")
                {
                    let plans = self
                        .conn
                        .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), params)?;
                    let details = plans
                        .iter()
                        .map(|row| row.get_as::<String>(3).unwrap())
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(
                        details.contains("USING INDEX sqlite_autoindex_"),
                        "{details}: {sql}"
                    );
                    // The pinned EXPLAIN QUERY PLAN labels a covering IN
                    // lookup as SCAN despite emitting one SeekGE per key.
                    // Check the actual program: no rewind/history walk, and
                    // one composite-index seek for every bounded literal ID.
                    let program = self.conn.query_sync(&format!("EXPLAIN {sql}"), params)?;
                    let opcodes = program
                        .iter()
                        .map(|row| row.get_as::<String>(1).unwrap())
                        .collect::<Vec<_>>();
                    let key_count = sql
                        .split_once("message_id IN (")
                        .unwrap()
                        .1
                        .split(')')
                        .next()
                        .unwrap()
                        .split(',')
                        .count();
                    assert!((1..=8).contains(&key_count));
                    assert_eq!(
                        opcodes
                            .iter()
                            .filter(|opcode| opcode.as_str() == "SeekGE")
                            .count(),
                        key_count,
                        "every requested key must drive its own index seek: {details}"
                    );
                    assert!(
                        opcodes
                            .iter()
                            .all(|opcode| !matches!(opcode.as_str(), "Rewind" | "Last")),
                        "receipt lookup must not start a table/index history walk: {details}"
                    );
                    assert!(rows.len() <= 8);
                }
                self.calls
                    .borrow_mut()
                    .push((sql.into(), params.to_vec(), rows.len()));
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.conn.execute_raw(sql)
            }
        }
        let conn = shared_reader_fixture(10_000);
        let observed = Observed {
            conn: &conn,
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let actual =
            fetch_project_mailbox_rows_from_conn(&observed, 1, 2, None, 50, options()).unwrap();
        let expected =
            reference_project_mailbox_rows_from_conn(&conn, 1, 2, None, 50, options()).unwrap();
        assert_eq!(actual.len(), 50);
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        let calls = observed.calls.borrow();
        assert!(calls.iter().all(|(sql, params, rows)| !sql.contains("JOIN")
            && params.len() <= 129
            && *rows <= 128));
        let pages: Vec<_> = calls
            .iter()
            .filter(|(sql, _, _)| sql.contains("ORDER BY created_ts DESC LIMIT 128"))
            .collect();
        assert_eq!(
            pages.len(),
            1,
            "a dense newest window must not walk historical deliveries"
        );
        for (sql, params, _) in pages {
            let plans = conn
                .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), params)
                .unwrap();
            let details = plans
                .iter()
                .map(|row| row.get_as::<String>(3).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                details.contains("SEARCH messages USING INDEX idx_messages_project_created"),
                "{details}"
            );
            assert!(!details.contains("TEMP B-TREE"), "{details}");
        }
    }

    #[test]
    fn mixed_direct_and_shared_inbox_preserves_one_ranked_window() {
        let conn = shared_reader_fixture(100);
        conn.execute_raw("UPDATE messages SET created_ts=((id * 17) % 101)*1000")
            .unwrap();
        let actual =
            crate::sync::fetch_inbox_rows_from_conn(&conn, 1, 2, false, false, false, None, 20)
                .unwrap();
        let mut expected: Vec<i64> = (1..=100).filter(|id| id % 5 != 0 || id % 7 == 0).collect();
        expected.sort_by_key(|id| std::cmp::Reverse(((id * 17) % 101, *id)));
        expected.truncate(20);
        assert_eq!(
            actual
                .iter()
                .map(|row| row.message.id.unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        for row in actual {
            assert_eq!(
                row.kind,
                if row.message.id.unwrap() % 7 == 0 {
                    "bcc"
                } else {
                    PROJECT_MAILBOX_KIND
                }
            );
            assert_eq!(
                row.message.body_md,
                format!("body-{}", row.message.id.unwrap())
            );
        }
    }

    #[test]
    fn shared_inbox_snapshot_keeps_membership_body_and_receipts_consistent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("shared-snapshot.sqlite3");
        let reader = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "shared reader",
        );
        reader.execute_raw(schema::PRAGMA_DB_INIT_SQL).unwrap();
        let cx = asupersync::Cx::for_testing();
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            schema::migrate_to_latest_base(&cx, &*reader)
                .await
                .into_result()
                .unwrap();
        });
        reader
            .execute_raw(
                "INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'one','/one',0);
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES
            (1,1,'BlueLake','test','test',0,0),(2,1,'GreenStone','test','test',0,0)",
            )
            .unwrap();
        seed_shared_reader(&reader, 1, 1);
        let writer = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "shared peer",
        );
        struct Racing<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            committed: std::cell::Cell<bool>,
        }
        impl crate::pool::SyncQuery for Racing<'_> {
            fn query_sync(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql.contains("ORDER BY created_ts DESC LIMIT 128")
                    && !self.committed.replace(true)
                {
                    self.writer.execute_raw("BEGIN IMMEDIATE;
                        UPDATE agents SET retired_at=99 WHERE id=2;
                        UPDATE messages SET body_md='changed body' WHERE id=1;
                        UPDATE project_mailbox_receipts SET read_ts=100,ack_ts=101 WHERE message_id=1;
                        COMMIT")?;
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
        }
        let race = Racing {
            reader: &reader,
            writer: &writer,
            committed: std::cell::Cell::new(false),
        };
        let filters = InboxFetchOptions {
            body_policy: InboxBodyPolicy::Full,
            ..options()
        };
        let old = fetch_project_mailbox_rows_from_conn(&race, 1, 2, None, 20, filters).unwrap();
        assert!(race.committed.get());
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].message.body_md, "body-1");
        assert_eq!((old[0].read_ts, old[0].ack_ts), (None, None));
        assert!(
            fetch_project_mailbox_rows_from_conn(&*reader, 1, 2, None, 20, filters)
                .unwrap()
                .is_empty()
        );
        writer
            .execute_raw("UPDATE agents SET retired_at=NULL WHERE id=2")
            .unwrap();
        let current =
            fetch_project_mailbox_rows_from_conn(&*reader, 1, 2, None, 20, filters).unwrap();
        assert_eq!(current[0].message.body_md, "changed body");
        assert_eq!(
            (current[0].read_ts, current[0].ack_ts),
            (Some(100), Some(101))
        );
    }

    #[test]
    #[ignore = "explicit same-invocation shared inbox 1k/10k scaling measurement"]
    fn shared_inbox_scaling_compares_live_join_at_both_history_sizes() {
        let conn = shared_reader_fixture(1_000);
        let mut medians = Vec::new();
        for count in [1_000, 10_000] {
            if count > 1_000 {
                seed_shared_reader(&conn, 1_001, count);
            }
            let mut candidate = Vec::new();
            let mut reference = Vec::new();
            for round in 0..5 {
                let new = || {
                    let start = std::time::Instant::now();
                    let rows =
                        fetch_project_mailbox_rows_from_conn(&conn, 1, 2, None, 50, options())
                            .unwrap();
                    (rows, start.elapsed().as_micros())
                };
                let old = || {
                    let start = std::time::Instant::now();
                    let rows =
                        reference_project_mailbox_rows_from_conn(&conn, 1, 2, None, 50, options())
                            .unwrap();
                    (rows, start.elapsed().as_micros())
                };
                let (actual, expected) = if round % 2 == 0 {
                    let a = new();
                    (a, old())
                } else {
                    let e = old();
                    (new(), e)
                };
                assert_eq!(format!("{:?}", actual.0), format!("{:?}", expected.0));
                candidate.push(actual.1);
                reference.push(expected.1);
            }
            candidate.sort_unstable();
            reference.sort_unstable();
            println!(
                "shared_deliveries={count} candidate_us={candidate:?} reference_us={reference:?}"
            );
            assert!(
                candidate[2] < reference[2],
                "new reader must beat the live join"
            );
            medians.push(candidate[2]);
        }
        assert!(
            medians[1] <= 2 * medians[0],
            "shared history still dominates page cost: {medians:?}"
        );
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
