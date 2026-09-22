//! Bounded, resumable observations for the overdue-acknowledgment worker.
//!
//! A scan lap freezes the message high-water mark and the overdue cutoff. New
//! mail cannot indefinitely extend it, and removing acknowledged rows cannot
//! shift an offset past unseen deliveries. The cursor includes both message
//! and recipient IDs because one message can span several pages.
//!
//! Metadata and deliveries come from one SQL statement. A different mailbox or
//! stamped database generation restarts the lap; no old page is replayed into
//! a new generation. Unstamped legacy databases remain readable, but cannot
//! provide the same generation-change detection. This is a bounded observation,
//! not a transaction authorizing a later mutation or a query execution deadline.

#[path = "ack_escalation.rs"]
mod escalation;
pub use escalation::{AckEscalationOutcome, AckEscalationRequest, grant_ack_escalation};

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Connection as _, Row, Value};

use crate::queries::UnackedMessageRow;
use crate::{DbError, DbPool, DbResult};

/// Maximum delivered rows in one observation, excluding its overflow witness.
pub const MAX_ACK_SCAN_PAGE_SIZE: usize = 128;

// The pinned engine evaluates a compound SELECT's outer LIMIT separately from
// its bound parameters (see the September 22 bridge integration receipt). Only
// the validated internal integer limit is appended below; every data value
// remains a binding with an explicit, statement-wide index.
const PAGE_SQL_PREFIX: &str = "\
WITH input AS (\
    SELECT ?1 AS resume, ?2 AS expected_generation, ?3 AS previous_upper_id, \
           ?4 AS previous_before_ts, ?5 AS current_before_ts, \
           ?6 AS previous_message, ?7 AS previous_agent\
), identity AS (\
    SELECT (SELECT generation_id FROM db_identity WHERE singleton = 0) AS generation_id\
), bounds AS (\
    SELECT generation_id, \
        CASE WHEN resume = 1 AND generation_id IS expected_generation THEN previous_upper_id \
             ELSE COALESCE((SELECT MAX(id) FROM messages), 0) END AS upper_id, \
        CASE WHEN resume = 1 AND generation_id IS expected_generation \
             THEN previous_before_ts ELSE current_before_ts END AS before_ts, \
        CASE WHEN resume = 1 AND generation_id IS expected_generation \
             THEN previous_message ELSE 0 END AS after_message, \
        CASE WHEN resume = 1 AND generation_id IS expected_generation \
             THEN previous_agent ELSE 0 END AS after_agent \
    FROM identity CROSS JOIN input\
) \
SELECT 0 AS row_kind, generation_id, upper_id, before_ts, after_message, after_agent, \
       0 AS message_id, 0 AS project_id, 0 AS created_ts, 0 AS agent_id FROM bounds \
UNION ALL \
SELECT 1, NULL, NULL, NULL, NULL, NULL, m.id, m.project_id, m.created_ts, mr.agent_id \
FROM messages m JOIN message_recipients mr ON mr.message_id = m.id CROSS JOIN bounds b \
WHERE m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts <= b.before_ts \
  AND m.id <= b.upper_id \
  AND (m.id > b.after_message OR (m.id = b.after_message AND mr.agent_id > b.after_agent)) \
ORDER BY row_kind, message_id, agent_id LIMIT ";

/// Opaque process-local continuation. It represents consumed deliveries, not a
/// message delivery cursor and not proof that an escalation succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckScanCursor {
    database_scope: String,
    generation_id: Option<String>,
    upper_id: i64,
    before_ts: i64,
    after_message: i64,
    after_agent: i64,
}

/// One bounded observation. Retaining a page across recovery is not supported;
/// callers retain only a continuation and read fresh rows for the next slice.
#[derive(Debug)]
pub struct AckScanPage {
    pub rows: Vec<UnackedMessageRow>,
    window: AckScanCursor,
    has_more: bool,
}

impl AckScanPage {
    /// Scope for the caller's bounded, process-local warning cache.
    #[must_use]
    pub fn database_scope(&self) -> &str {
        &self.window.database_scope
    }

    #[must_use]
    pub fn generation_id(&self) -> Option<&str> {
        self.window.generation_id.as_deref()
    }

    /// Frozen eligibility bound to carry into the grant transaction. Resuming
    /// a lap does not replace this with the latest wall-clock cutoff.
    #[must_use]
    pub const fn overdue_before_ts(&self) -> i64 {
        self.window.before_ts
    }

    /// Advance only past rows the caller actually handled. Zero preserves the
    /// window without consuming its head. A complete lap returns `None`, so the
    /// next observation captures a fresh cutoff and high-water mark.
    pub fn continuation_after(&self, consumed: usize) -> DbResult<Option<AckScanCursor>> {
        if consumed > self.rows.len() {
            return Err(scan_error("consumed count exceeds the observed page"));
        }
        if consumed == self.rows.len() && !self.has_more {
            return Ok(None);
        }
        let mut cursor = self.window.clone();
        if let Some(row) = consumed
            .checked_sub(1)
            .and_then(|index| self.rows.get(index))
        {
            cursor.after_message = row.message_id;
            cursor.after_agent = row.agent_id;
        }
        Ok(Some(cursor))
    }
}

fn scan_error(detail: &str) -> DbError {
    DbError::Sqlite(format!("ACK_SCAN: {detail}"))
}

fn page_query(
    database_scope: &str,
    cursor: Option<&AckScanCursor>,
    overdue_before_ts: i64,
    page_size: usize,
) -> DbResult<(String, Vec<Value>)> {
    if !(1..=MAX_ACK_SCAN_PAGE_SIZE).contains(&page_size) {
        return Err(scan_error("page size must be between 1 and 128"));
    }
    let cursor = cursor.filter(|cursor| cursor.database_scope == database_scope);
    let generation = cursor
        .and_then(|cursor| cursor.generation_id.clone())
        .map_or(Value::Null, Value::Text);
    // The check above makes addition bounded (3..=130). No external string is
    // interpolated, including generation IDs that contain SQL punctuation.
    let sql = format!("{PAGE_SQL_PREFIX}{}", page_size + 2);
    let params = vec![
        i64::from(cursor.is_some()).into(),
        generation,
        cursor.map_or(0, |cursor| cursor.upper_id).into(),
        cursor
            .map_or(overdue_before_ts, |cursor| cursor.before_ts)
            .into(),
        overdue_before_ts.into(),
        cursor.map_or(0, |cursor| cursor.after_message).into(),
        cursor.map_or(0, |cursor| cursor.after_agent).into(),
    ];
    Ok((sql, params))
}

fn decode_page(database_scope: String, rows: Vec<Row>, page_size: usize) -> DbResult<AckScanPage> {
    if rows.is_empty() || rows.len() > page_size + 2 {
        return Err(scan_error("missing scan metadata or oversized projection"));
    }
    let header = &rows[0];
    let integer = |name| {
        header
            .get_named::<i64>(name)
            .map_err(|error| DbError::Sqlite(error.to_string()))
    };
    if integer("row_kind")? != 0 {
        return Err(scan_error("scan metadata is not first"));
    }
    let generation_id = header
        .get_named::<Option<String>>("generation_id")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    if generation_id
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.len() > 256)
    {
        return Err(scan_error("database generation is empty or oversized"));
    }
    let window = AckScanCursor {
        database_scope,
        generation_id,
        upper_id: integer("upper_id")?,
        before_ts: integer("before_ts")?,
        after_message: integer("after_message")?,
        after_agent: integer("after_agent")?,
    };
    if window.upper_id < 0 || window.after_message < 0 || window.after_agent < 0 {
        return Err(scan_error("negative scan bounds"));
    }
    let mut previous = (window.after_message, window.after_agent);
    let mut messages = Vec::with_capacity(rows.len() - 1);
    for row in rows.iter().skip(1) {
        let integer = |name| {
            row.get_named::<i64>(name)
                .map_err(|error| DbError::Sqlite(error.to_string()))
        };
        let message = UnackedMessageRow {
            message_id: integer("message_id")?,
            project_id: integer("project_id")?,
            created_ts: integer("created_ts")?,
            agent_id: integer("agent_id")?,
        };
        let key = (message.message_id, message.agent_id);
        if integer("row_kind")? != 1
            || message.message_id <= 0
            || message.agent_id <= 0
            || message.project_id <= 0
            || key <= previous
            || message.message_id > window.upper_id
            || message.created_ts > window.before_ts
        {
            return Err(scan_error("unordered or out-of-window delivery projection"));
        }
        previous = key;
        messages.push(message);
    }
    // Validate the witness too, but never hand it to the worker as consumed work.
    let has_more = messages.len() > page_size;
    messages.truncate(page_size);
    Ok(AckScanPage {
        rows: messages,
        window,
        has_more,
    })
}

/// Read one fresh page using the runtime mailbox connection and caller context.
///
/// The result contains no subjects, bodies, recipient names, or attachment data.
/// Callers performing escalations must keep their existing generation/write
/// lease through that page's actions, then release it before yielding.
pub async fn overdue_ack_page(
    cx: &Cx,
    pool: &DbPool,
    cursor: Option<&AckScanCursor>,
    overdue_before_ts: i64,
    page_size: usize,
) -> Outcome<AckScanPage, DbError> {
    let scope = pool.sqlite_identity_key();
    let (sql, params) = match page_query(&scope, cursor, overdue_before_ts, page_size) {
        Ok(query) => query,
        Err(error) => return Outcome::Err(error),
    };
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let started = crate::tracking::query_timer();
    let result = conn.query(cx, &sql, &params).await;
    crate::tracking::record_query(&sql, crate::tracking::elapsed_us(started));
    match result {
        Outcome::Ok(rows) => match decode_page(scope, rows, page_size) {
            Ok(page) => Outcome::Ok(page),
            Err(error) => Outcome::Err(error),
        },
        Outcome::Err(error) => Outcome::Err(DbError::Sqlite(error.to_string())),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(test: impl FnOnce(&crate::DbConn)) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("acks.sqlite3");
        let conn = crate::guard_db_conn(
            crate::DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "ack scan fixture",
        );
        for sql in [
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, created_ts INTEGER NOT NULL, ack_required INTEGER NOT NULL)",
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, ack_ts INTEGER, PRIMARY KEY (message_id, agent_id))",
            "CREATE TABLE db_identity (singleton INTEGER PRIMARY KEY, generation_id TEXT NOT NULL)",
            "INSERT INTO db_identity VALUES (0, 'generation-one')",
        ] {
            conn.execute_sync(sql, &[]).unwrap();
        }
        test(&conn);
    }

    fn seed(conn: &crate::DbConn, message: i64, recipients: &[i64], created: i64) {
        conn.execute_sync(
            "INSERT INTO messages VALUES (?, 1, ?, 1)",
            &[message.into(), created.into()],
        )
        .unwrap();
        for recipient in recipients {
            conn.execute_sync(
                "INSERT INTO message_recipients VALUES (?, ?, NULL)",
                &[message.into(), (*recipient).into()],
            )
            .unwrap();
        }
    }

    fn page(
        conn: &crate::DbConn,
        scope: &str,
        cursor: Option<&AckScanCursor>,
        before: i64,
        size: usize,
    ) -> AckScanPage {
        let (sql, params) = page_query(scope, cursor, before, size).unwrap();
        let rows = conn.query_sync(&sql, &params).unwrap();
        decode_page(scope.to_string(), rows, size).unwrap()
    }

    fn keys(page: &AckScanPage) -> Vec<(i64, i64)> {
        page.rows
            .iter()
            .map(|row| (row.message_id, row.agent_id))
            .collect()
    }

    #[test]
    fn one_message_many_recipients_crosses_pages_without_loss() {
        fixture(|conn| {
            seed(conn, 1, &[1, 2, 3, 4, 5], 10);
            let first = page(conn, "db", None, 10, 2);
            assert_eq!(keys(&first), vec![(1, 1), (1, 2)]);
            let cursor = first.continuation_after(2).unwrap().unwrap();
            let second = page(conn, "db", Some(&cursor), 10, 2);
            assert_eq!(keys(&second), vec![(1, 3), (1, 4)]);
            let cursor = second.continuation_after(2).unwrap().unwrap();
            let third = page(conn, "db", Some(&cursor), 10, 2);
            assert_eq!(keys(&third), vec![(1, 5)]);
            assert!(third.continuation_after(1).unwrap().is_none());
        });
    }

    #[test]
    fn partial_page_advances_only_consumed_rows() {
        fixture(|conn| {
            seed(conn, 1, &[1, 2, 3, 4], 10);
            let first = page(conn, "db", None, 10, 3);
            let zero = first.continuation_after(0).unwrap().unwrap();
            assert_eq!(keys(&page(conn, "db", Some(&zero), 999, 3)), keys(&first));
            let one = first.continuation_after(1).unwrap().unwrap();
            assert_eq!(
                keys(&page(conn, "db", Some(&one), 999, 3)),
                vec![(1, 2), (1, 3), (1, 4)]
            );
            assert!(first.continuation_after(4).is_err());
        });
    }

    #[test]
    fn incoming_messages_cannot_extend_the_current_lap() {
        fixture(|conn| {
            seed(conn, 1, &[1], 10);
            seed(conn, 2, &[1], 10);
            let first = page(conn, "db", None, 10, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            seed(conn, 3, &[1], 10);
            let second = page(conn, "db", Some(&cursor), 10, 1);
            assert_eq!(keys(&second), vec![(2, 1)]);
            assert!(second.continuation_after(1).unwrap().is_none());
            assert_eq!(
                keys(&page(conn, "db", None, 10, 3)),
                vec![(1, 1), (2, 1), (3, 1)]
            );
        });
    }

    #[test]
    fn cutoff_is_frozen_and_acknowledgments_do_not_shift_offsets() {
        fixture(|conn| {
            for message in 1..=4 {
                seed(conn, message, &[1], message * 10);
            }
            let first = page(conn, "db", None, 30, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            conn.execute_sync(
                "UPDATE message_recipients SET ack_ts = 50 WHERE message_id <= 2",
                &[],
            )
            .unwrap();
            let second = page(conn, "db", Some(&cursor), 999, 2);
            assert_eq!(keys(&second), vec![(3, 1)]);
            assert_eq!(first.overdue_before_ts(), 30);
            assert_eq!(second.overdue_before_ts(), 30);
            assert!(second.continuation_after(1).unwrap().is_none());
            assert_eq!(keys(&page(conn, "db", None, 999, 4)), vec![(3, 1), (4, 1)]);
        });
    }

    #[test]
    fn changed_generation_restarts_instead_of_skipping_reused_ids() {
        fixture(|conn| {
            seed(conn, 1, &[1, 2, 3], 10);
            let first = page(conn, "db", None, 10, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            conn.execute_sync(
                "UPDATE db_identity SET generation_id = 'generation-two' WHERE singleton = 0",
                &[],
            )
            .unwrap();
            let restarted = page(conn, "db", Some(&cursor), 20, 1);
            assert_eq!(keys(&restarted), vec![(1, 1)]);
            assert_eq!(restarted.generation_id(), Some("generation-two"));
            assert_eq!(restarted.overdue_before_ts(), 20);
        });
    }

    #[test]
    fn another_database_cannot_inherit_a_cursor_with_the_same_generation() {
        fixture(|conn| {
            seed(conn, 1, &[1, 2], 10);
            let first = page(conn, "one", None, 10, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            let second = page(conn, "two", Some(&cursor), 20, 1);
            assert_eq!(keys(&second), vec![(1, 1)]);
            assert_eq!(second.database_scope(), "two");
        });
    }

    #[test]
    fn maximum_ids_and_empty_pages_finish_without_cursor_overflow() {
        fixture(|conn| {
            assert!(
                page(conn, "db", None, 10, 1)
                    .continuation_after(0)
                    .unwrap()
                    .is_none()
            );
            seed(conn, i64::MAX, &[1, i64::MAX], 10);
            let first = page(conn, "db", None, 10, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            let second = page(conn, "db", Some(&cursor), 10, 1);
            assert_eq!(keys(&second), vec![(i64::MAX, i64::MAX)]);
            assert!(second.continuation_after(1).unwrap().is_none());
        });
    }

    #[test]
    fn page_size_is_bounded_before_querying() {
        for size in [0, MAX_ACK_SCAN_PAGE_SIZE + 1, usize::MAX] {
            assert!(page_query("db", None, 10, size).is_err());
        }
        assert!(page_query("db", None, 10, MAX_ACK_SCAN_PAGE_SIZE).is_ok());
    }

    #[test]
    fn missing_generation_record_remains_readable_but_bad_generation_is_refused() {
        fixture(|conn| {
            conn.execute_sync("UPDATE db_identity SET singleton = 1", &[])
                .unwrap();
            seed(conn, 1, &[1, 2], 10);
            let first = page(conn, "db", None, 10, 1);
            assert_eq!(first.generation_id(), None);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            let second = page(conn, "db", Some(&cursor), 10, 1);
            assert_eq!(keys(&second), vec![(1, 2)]);
            conn.execute_sync(
                "UPDATE db_identity SET singleton = 0, generation_id = ''",
                &[],
            )
            .unwrap();
            let (sql, params) = page_query("db", None, 10, 1).unwrap();
            assert!(decode_page("db".into(), conn.query_sync(&sql, &params).unwrap(), 1).is_err());
        });
    }

    #[test]
    fn compound_limit_is_internal_and_all_data_bindings_are_explicit() {
        for size in 1..=MAX_ACK_SCAN_PAGE_SIZE {
            let (sql, params) = page_query("db", None, i64::MIN, size).unwrap();
            assert_eq!(params.len(), 7);
            assert_eq!(
                sql.strip_prefix(PAGE_SQL_PREFIX),
                Some((size + 2).to_string().as_str())
            );
            assert!(!sql.contains("LIMIT ?"));
            for index in 1..=7 {
                assert_eq!(sql.matches(&format!("?{index} ")).count(), 1);
            }
        }
    }

    #[test]
    fn every_supported_page_size_keeps_exactly_one_overflow_witness() {
        fixture(|conn| {
            let recipients: Vec<i64> = (1..=130).collect();
            seed(conn, 1, &recipients, 10);
            for size in 1..=MAX_ACK_SCAN_PAGE_SIZE {
                let (sql, params) = page_query("db", None, 10, size).unwrap();
                let rows = conn.query_sync(&sql, &params).unwrap();
                assert_eq!(rows.len(), size + 2, "metadata plus page plus witness");
                let observed = decode_page("db".into(), rows, size).unwrap();
                assert_eq!(observed.rows.len(), size);
                assert!(observed.has_more);
                assert!(observed.continuation_after(size + 1).is_err());
                let cursor = observed.continuation_after(size).unwrap().unwrap();
                let next = page(conn, "db", Some(&cursor), 999, size);
                assert_eq!(next.rows[0].agent_id, i64::try_from(size + 1).unwrap());
                assert_eq!(next.overdue_before_ts(), 10);
            }
        });
    }

    #[test]
    fn quoted_generation_is_data_on_both_compound_arms() {
        fixture(|conn| {
            let generation = "generation' UNION SELECT ?8; -- 🦀";
            conn.execute_sync(
                "UPDATE db_identity SET generation_id = ?1 WHERE singleton = 0",
                &[generation.into()],
            )
            .unwrap();
            seed(conn, 7, &[11, 13, 17], 19);
            let first = page(conn, "mailbox", None, 23, 1);
            let cursor = first.continuation_after(1).unwrap().unwrap();
            let (sql, params) = page_query("mailbox", Some(&cursor), 29, 1).unwrap();
            assert!(!sql.contains(generation));
            assert_eq!(params.len(), 7);
            let next =
                decode_page("mailbox".into(), conn.query_sync(&sql, &params).unwrap(), 1).unwrap();
            assert_eq!(keys(&next), vec![(7, 13)]);
            assert_eq!(next.generation_id(), Some(generation));
            assert_eq!(next.overdue_before_ts(), 23);
            assert_eq!(next.window.upper_id, 7);
        });
    }
}
