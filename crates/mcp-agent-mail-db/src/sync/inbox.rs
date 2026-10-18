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
pub const RECIPIENT_PAGE_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
    FROM message_recipients WHERE agent_id = ? ORDER BY message_id ASC LIMIT 256";
pub const RECIPIENT_NEXT_SQL: &str = "SELECT message_id, kind, read_ts, ack_ts \
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
    i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut after = None;
    let mut newest: BTreeMap<(i64, i64), Receipt> = BTreeMap::new();
    loop {
        let (sql, params) = after.map_or_else(
            || (RECIPIENT_PAGE_SQL, vec![Value::BigInt(agent_id)]),
            |id| {
                (
                    RECIPIENT_NEXT_SQL,
                    vec![Value::BigInt(agent_id), Value::BigInt(id)],
                )
            },
        );
        let rows = query(conn, sql, &params)?;
        if rows.len() > PAGE_SIZE {
            return Err(DbError::Internal(
                "inbox recipient page exceeded its bound".into(),
            ));
        }
        let finished = rows.len() < PAGE_SIZE;
        let mut receipts = HashMap::new();
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.get_named::<i64>("message_id").map_err(db_error)?;
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
            ids.push(id);
            receipts.insert(id, receipt);
        }
        if !ids.is_empty() {
            let (sql, params) = lookup_query("messages", HEADER_COLUMNS, &ids);
            for row in query(conn, &sql, &params)? {
                let id = row.get_named::<i64>("id").map_err(db_error)?;
                let receipt = receipts.remove(&id).ok_or_else(|| {
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
                if newest.len() < limit
                    || newest.first_key_value().is_some_and(|(old, _)| key > *old)
                {
                    newest.insert(key, receipt);
                    if newest.len() > limit {
                        newest.pop_first();
                    }
                }
            }
            // Missing message rows are orphan recipients, excluded by the old
            // inner join too. They still advance the recipient scan above.
        }
        if finished {
            break;
        }
    }
    hydrate(conn, project_id, newest, options.body_policy)
}

fn hydrate(
    conn: &impl SyncQuery,
    project_id: i64,
    newest: BTreeMap<(i64, i64), Receipt>,
    body_policy: InboxBodyPolicy,
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
    let columns = format!(
        "id, project_id, sender_id, thread_id, topic, subject, {body}, \
        importance, ack_required, created_ts, recipients_json, attachments"
    );
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
    hydrate(conn, project_id, newest, options.body_policy)
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
        assert_eq!(
            calls
                .iter()
                .filter(|(sql, _, _)| sql.contains("FROM message_recipients"))
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

    #[test]
    fn indexed_queries_use_recipient_range_and_message_primary_keys() {
        let conn = seed(5);
        for sql in [RECIPIENT_PAGE_SQL, RECIPIENT_NEXT_SQL] {
            let rows = conn
                .query_sync(
                    &format!("EXPLAIN QUERY PLAN {sql}"),
                    &[2_i64.into(), 0_i64.into()],
                )
                .unwrap();
            let details = rows
                .iter()
                .map(|row| row.get_as::<String>(3).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                details.contains(
                    "SEARCH message_recipients USING INDEX idx_message_recipients_agent_message"
                ),
                "{details}"
            );
        }
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
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::*;
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
}
