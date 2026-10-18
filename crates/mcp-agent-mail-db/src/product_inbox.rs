//! Scoped product inbox fan-in over indexed per-viewer mailbox readers.
//!
//! Authorization is performed by the caller before this read. Only the exact
//! preflighted agent IDs may participate; current names and product membership
//! are checked again inside the same snapshot as all messages and receipts.
//! No broad product/message join, fresh connection per project, or partial
//! success on a failed project is permitted. Bodies are loaded only after the
//! global newest window has been selected.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use asupersync::{CancelReason, Cx, Outcome};
use sqlmodel_core::{Error as SqlError, Row, Value};

use crate::pool::SyncQuery;
use crate::queries::InboxRow;
use crate::sync::inbox;
use crate::{DbConn, DbError, DbPool};

const LOOKUP_BATCH: usize = 256;
pub(crate) const MEMBERSHIP_SQL: &str = "SELECT project_id FROM product_project_links \
    WHERE product_id = ? AND project_id = ? LIMIT 1";

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
            .unwrap_or_else(|| CancelReason::user("product inbox read cancelled")),
    )
}

/// A failed or unwound multi-statement read must not return a possibly active
/// savepoint to the pool. Return the checkout only after successful cleanup.
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
                tracing::warn!(%error, "product inbox failed checkout retirement");
            }
        }
    }
}

/// Fetch one newest-first product window through an already-admitted pool.
///
/// `viewer_ids` must be the exact identities authorized during preflight.
/// A new same-name agent or product link cannot expand this set. Explicit
/// delivery remains readable to retired/blocked agents; shared delivery keeps
/// the ordinary mailbox visibility policy. Duplicate supplied IDs are harmless.
/// Cancellation returns `Outcome::Cancelled`, never a truncated success.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_inbox_for_product_agent_scoped(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    agent_name: &str,
    viewer_ids: &[i64],
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    include_bodies: bool,
) -> Outcome<Vec<InboxRow>, DbError> {
    if cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    if i64::try_from(limit).is_err() {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    }
    if viewer_ids.is_empty() || limit == 0 {
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
    let result = read_from_conn(
        cx,
        &**checkout
            .connection
            .as_ref()
            .expect("live product read checkout"),
        product_id,
        agent_name,
        viewer_ids,
        urgent_only,
        since_ts,
        limit,
        include_bodies,
    );
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
        // Cleanup cannot be cancelled: every nested reader uses this exact
        // savepoint. RELEASE ends only our innermost scope, not a caller's tx.
        if sql != "RELEASE SAVEPOINT am_inbox_read" {
            self.checkpoint()?;
        }
        self.connection.execute_raw(sql)
    }
}

#[allow(clippy::too_many_arguments)]
fn read_from_conn(
    cx: &Cx,
    conn: &impl SyncQuery,
    product_id: i64,
    agent_name: &str,
    viewer_ids: &[i64],
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    include_bodies: bool,
) -> Outcome<Vec<InboxRow>, DbError> {
    if cx.checkpoint().is_err() {
        return cancelled(cx);
    }
    if i64::try_from(limit).is_err() {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    }
    if viewer_ids.is_empty() || limit == 0 {
        return Outcome::Ok(Vec::new());
    }
    let checked = CancellableRead {
        connection: conn,
        cx,
        interrupted: Cell::new(false),
    };
    let result = inbox::with_snapshot(&checked, || {
        let viewers = current_viewers(&checked, product_id, agent_name, viewer_ids)?;
        let mut window = BTreeMap::new();
        for (project, viewer) in viewers {
            let rows = crate::sync::fetch_inbox_metadata_rows_from_conn(
                &checked,
                project,
                viewer,
                urgent_only,
                false,
                false,
                since_ts,
                limit,
            )?;
            for row in rows {
                let id = row
                    .message
                    .id
                    .ok_or_else(|| DbError::Internal("product inbox message has no ID".into()))?;
                // A view is keyed separately: distinct authorized legacy
                // identities may each have their own receipt for the same ID.
                window.insert((row.message.created_ts, id, viewer), row);
                if window.len() > limit {
                    window.pop_first();
                }
            }
        }
        let mut out: Vec<InboxRow> = window.into_values().rev().collect();
        if include_bodies {
            hydrate_bodies(&checked, &mut out)?;
        }
        Ok(out)
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

fn current_viewers(
    conn: &impl SyncQuery,
    product_id: i64,
    agent_name: &str,
    viewer_ids: &[i64],
) -> Result<BTreeSet<(i64, i64)>, DbError> {
    let ids: Vec<i64> = viewer_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut memberships = HashMap::new();
    let mut viewers = BTreeSet::new();
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let (sql, params) = inbox::lookup_query("agents", "id, project_id, name", chunk);
        let rows = conn.query_sync(&sql, &params).map_err(db_error)?;
        let mut observed = BTreeSet::new();
        for row in rows {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            if chunk.binary_search(&id).is_err() || !observed.insert(id) {
                return Err(DbError::Internal(
                    "unrequested or duplicate product inbox viewer".into(),
                ));
            }
            let name = row.get_named::<String>("name").map_err(db_error)?;
            if !name.eq_ignore_ascii_case(agent_name) {
                continue;
            }
            let project = row.get_named::<i64>("project_id").map_err(db_error)?;
            let linked = if let Some(linked) = memberships.get(&project) {
                *linked
            } else {
                let links = conn
                    .query_sync(
                        MEMBERSHIP_SQL,
                        &[Value::BigInt(product_id), Value::BigInt(project)],
                    )
                    .map_err(db_error)?;
                if links.len() > 1
                    || links
                        .first()
                        .is_some_and(|row| row.get_named::<i64>("project_id").ok() != Some(project))
                {
                    return Err(DbError::Internal(
                        "invalid product membership observation".into(),
                    ));
                }
                let linked = !links.is_empty();
                memberships.insert(project, linked);
                linked
            };
            if linked {
                viewers.insert((project, id));
            }
        }
    }
    Ok(viewers)
}

fn hydrate_bodies(conn: &impl SyncQuery, rows: &mut [InboxRow]) -> Result<(), DbError> {
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.message.id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut bodies = HashMap::new();
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let (sql, params) =
            inbox::lookup_query("messages", "id, project_id, created_ts, body_md", chunk);
        for row in conn.query_sync(&sql, &params).map_err(db_error)? {
            let id = row.get_named::<i64>("id").map_err(db_error)?;
            let body = (
                row.get_named::<i64>("project_id").map_err(db_error)?,
                row.get_named::<i64>("created_ts").map_err(db_error)?,
                row.get_named::<String>("body_md").map_err(db_error)?,
            );
            if chunk.binary_search(&id).is_err() || bodies.insert(id, body).is_some() {
                return Err(DbError::Internal(
                    "unrequested or duplicate product inbox body".into(),
                ));
            }
        }
    }
    for row in rows {
        let id = row
            .message
            .id
            .ok_or_else(|| DbError::Internal("product inbox message has no ID".into()))?;
        let (project, created, body) = bodies.get(&id).ok_or_else(|| {
            DbError::Internal("product inbox body disappeared inside its snapshot".into())
        })?;
        if *project != row.message.project_id || *created != row.message.created_ts {
            return Err(DbError::Internal(
                "product inbox body changed inside its snapshot".into(),
            ));
        }
        row.message.body_md.clone_from(body);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::time::Instant;

    fn run<F: Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn fixture() -> (tempfile::TempDir, crate::DbConnGuard, DbPool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("product-inbox.sqlite3");
        let conn = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "product inbox fixture",
        );
        conn.execute_raw(crate::schema::PRAGMA_DB_INIT_SQL).unwrap();
        run(async {
            crate::schema::migrate_to_latest_base(&Cx::for_testing(), &*conn)
                .await
                .into_result()
                .unwrap();
        });
        conn.execute_raw("INSERT INTO products(id,product_uid,name,created_at) VALUES(1,'product-one','One',0),(2,'product-two','Two',0);
            INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'one','/one',0),(2,'two','/two',0),(3,'three','/three',0);
            INSERT INTO product_project_links(product_id,project_id,created_at) VALUES(1,1,0),(1,2,0),(2,3,0);
            INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES
            (11,1,'GreenStone','test','test',0,0),(12,1,'BlueLake','test','test',0,0),
            (21,2,'RedFox','test','test',0,0),(22,2,'bluelake','test','test',0,0),
            (31,3,'GoldFox','test','test',0,0),(32,3,'BlueLake','test','test',0,0);
            BEGIN").unwrap();
        for project in 1..=3 {
            for n in 1..=30 {
                let id = project * 10_000 + n;
                let sender = project * 10 + if n % 5 == 0 { 2 } else { 1 };
                conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts,importance,ack_required,topic)
                    VALUES({id},{project},{sender},'subject-{id}','body-{id}',{},'{}',{},'Handoffs')",
                    n * 1000 + project * 10, if n % 4 == 0 { "urgent" } else { "normal" }, n % 2)).unwrap();
                if n % 3 != 1 {
                    conn.execute_raw(&format!(
                        "INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts)
                        VALUES({id},{},'bcc',{},NULL)",
                        project * 10 + 2,
                        if n % 2 == 0 { "5000" } else { "NULL" }
                    ))
                    .unwrap();
                }
                if n % 3 != 0 {
                    conn.execute_raw(&format!("INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts)
                        VALUES({id},{project},'to',{}); INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts)
                        VALUES({id},{},NULL,{})",100_000-n,project * 10 + 2,if n%2==0{"6000"}else{"NULL"})).unwrap();
                }
            }
        }
        conn.execute_raw("COMMIT").unwrap();
        let config = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", path.display()),
            min_connections: 0,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&config).unwrap();
        (directory, conn, pool)
    }

    fn read(
        cx: &Cx,
        conn: &impl SyncQuery,
        ids: &[i64],
        limit: usize,
        bodies: bool,
    ) -> Outcome<Vec<InboxRow>, DbError> {
        read_from_conn(cx, conn, 1, "BLUELAKE", ids, false, None, limit, bodies)
    }

    #[test]
    fn large_preflight_scope_and_body_windows_cross_lookup_boundaries() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("BEGIN").unwrap();
        for start in (1..=300).step_by(128) {
            let end = (start + 127).min(300);
            let messages = (start..=end)
                .map(|n| {
                    format!(
                        "({},1,11,'large','large-body-{}',{})",
                        40000 + n,
                        n,
                        1_000_000 + n
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let recipients = (start..=end)
                .map(|n| format!("({},12,'to')", 40000 + n))
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts) VALUES {messages};
                INSERT INTO message_recipients(message_id,agent_id,kind) VALUES {recipients}")).unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let mut ids: Vec<i64> = (1000..1600).collect();
        ids.extend([12, 22, 12]);
        let rows = read(&Cx::for_testing(), &*conn, &ids, 300, true)
            .into_result()
            .unwrap();
        assert_eq!(rows.len(), 300);
        for (row, n) in rows.iter().zip((1..=300).rev()) {
            assert_eq!(row.message.id, Some(40000 + n));
            assert_eq!(row.message.body_md, format!("large-body-{n}"));
        }
        #[cfg(target_pointer_width = "64")]
        assert!(matches!(
            read(&Cx::for_testing(), &*conn, &[], usize::MAX, false),
            Outcome::Err(DbError::InvalidArgument { field: "limit", .. })
        ));
    }

    #[test]
    fn public_reader_matches_original_fan_in_for_filters_limits_bodies_and_receipts() {
        let (_directory, _conn, pool) = fixture();
        run(async {
            let cx = Cx::for_testing();
            for urgent in [false, true] {
                for since in [None, Some(20_000)] {
                    for limit in [0, 1, 7, 100] {
                        for bodies in [false, true] {
                            let expected = crate::queries::fetch_inbox_for_product_agent_scoped(
                                &cx,
                                &pool,
                                1,
                                "BLUELAKE",
                                &[22, 12, 12],
                                urgent,
                                since,
                                limit,
                                bodies,
                            )
                            .await
                            .into_result()
                            .unwrap();
                            let actual = fetch_inbox_for_product_agent_scoped(
                                &cx,
                                &pool,
                                1,
                                "BLUELAKE",
                                &[22, 12, 12],
                                urgent,
                                since,
                                limit,
                                bodies,
                            )
                            .await
                            .into_result()
                            .unwrap();
                            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
                            assert!(actual.iter().all(|row| row.message.project_id != 3));
                            if !bodies {
                                assert!(actual.iter().all(|row| row.message.body_md.is_empty()));
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn membership_name_and_exact_preflight_ids_limit_the_scope() {
        let (_directory, conn, _pool) = fixture();
        let cx = Cx::for_testing();
        conn.execute_raw(
            "DELETE FROM product_project_links WHERE product_id=1 AND project_id=2;
            INSERT INTO product_project_links(product_id,project_id,created_at) VALUES(1,3,1)",
        )
        .unwrap();
        let rows = read(&cx, &*conn, &[12, 22], 100, true)
            .into_result()
            .unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row.message.project_id == 1));
        conn.execute_raw("UPDATE agents SET name='PurpleHill' WHERE id=12")
            .unwrap();
        assert!(
            read(&cx, &*conn, &[12, 22], 100, true)
                .into_result()
                .unwrap()
                .is_empty()
        );
        assert!(
            read(&cx, &*conn, &[], 100, true)
                .into_result()
                .unwrap()
                .is_empty()
        );
        let fresh_scope = read(&cx, &*conn, &[32], 100, true).into_result().unwrap();
        assert!(!fresh_scope.is_empty());
        assert!(fresh_scope.iter().all(|row| row.message.project_id == 3));
    }

    #[test]
    fn retired_or_blocked_viewers_keep_direct_but_not_shared_deliveries() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw(
            "UPDATE agents SET retired_at=1 WHERE id=12;
            UPDATE agents SET contact_policy='BLOCK_ALL' WHERE id=22",
        )
        .unwrap();
        let rows = read(&Cx::for_testing(), &*conn, &[12, 22], 100, true)
            .into_result()
            .unwrap();
        assert_eq!(rows.len(), 40);
        assert!(rows.iter().all(|row| row.kind == "bcc"));
    }

    #[test]
    fn global_window_hydrates_only_returned_bodies_and_uses_no_history_join() {
        struct Recording<'a> {
            conn: &'a DbConn,
            queries: RefCell<Vec<(String, usize)>>,
        }
        impl SyncQuery for Recording<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.conn.query_sync(sql, params)?;
                self.queries.borrow_mut().push((sql.into(), rows.len()));
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.conn.execute_raw(sql)
            }
        }
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("PRAGMA query_only=ON").unwrap();
        let recording = Recording {
            conn: &conn,
            queries: RefCell::new(Vec::new()),
        };
        let rows = read(&Cx::for_testing(), &recording, &[12, 22], 7, true)
            .into_result()
            .unwrap();
        assert_eq!(rows.len(), 7);
        assert!(
            rows.iter()
                .all(|row| row.message.body_md == format!("body-{}", row.message.id.unwrap()))
        );
        let queries = recording.queries.borrow();
        assert!(
            queries
                .iter()
                .all(|(sql, count)| !sql.contains("JOIN") && *count <= LOOKUP_BATCH)
        );
        assert_eq!(
            queries
                .iter()
                .filter(|(sql, _)| sql.contains("created_ts, body_md FROM messages"))
                .map(|(_, count)| count)
                .sum::<usize>(),
            7
        );
        assert_eq!(
            conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn peer_commit_cannot_mix_product_links_bodies_and_receipts() {
        let (directory, conn, _pool) = fixture();
        let writer = crate::guard_db_conn(
            DbConn::open_file(
                directory
                    .path()
                    .join("product-inbox.sqlite3")
                    .to_str()
                    .unwrap(),
            )
            .unwrap(),
            "product inbox peer",
        );
        struct Racing<'a> {
            reader: &'a DbConn,
            writer: &'a DbConn,
            committed: Cell<bool>,
        }
        impl SyncQuery for Racing<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql == inbox::RECIPIENT_PAGE_SQL && !self.committed.replace(true) {
                    self.writer.execute_raw(
                        "BEGIN IMMEDIATE;
                        DELETE FROM product_project_links WHERE product_id=1 AND project_id=2;
                        UPDATE messages SET body_md='new peer body' WHERE project_id=2;
                        UPDATE message_recipients SET read_ts=99 WHERE agent_id=22;
                        UPDATE project_mailbox_receipts SET read_ts=99 WHERE agent_id=22;
                        COMMIT",
                    )?;
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.reader.execute_raw(sql)
            }
        }
        let race = Racing {
            reader: &conn,
            writer: &writer,
            committed: Cell::new(false),
        };
        let cx = Cx::for_testing();
        let before = read(&cx, &race, &[12, 22], 100, true)
            .into_result()
            .unwrap();
        assert!(race.committed.get());
        assert!(before.iter().any(|row| row.message.project_id == 2));
        assert!(
            before
                .iter()
                .all(|row| row.read_ts != Some(99) && row.message.body_md.starts_with("body-"))
        );
        let after = read(&cx, &*conn, &[12, 22], 100, true)
            .into_result()
            .unwrap();
        assert!(after.iter().all(|row| row.message.project_id == 1));
        writer
            .execute_raw(
                "INSERT INTO product_project_links(product_id,project_id,created_at) VALUES(1,2,1)",
            )
            .unwrap();
        let current = read(&cx, &*conn, &[12, 22], 100, true)
            .into_result()
            .unwrap();
        assert!(
            current
                .iter()
                .filter(|row| row.message.project_id == 2)
                .all(|row| row.message.body_md == "new peer body" && row.read_ts == Some(99))
        );
    }

    #[test]
    fn cancellation_after_a_real_page_releases_only_our_read_scopes() {
        struct Interrupt<'a> {
            conn: &'a DbConn,
            cx: &'a Cx,
            fired: Cell<bool>,
        }
        impl SyncQuery for Interrupt<'_> {
            fn query_sync(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, SqlError> {
                let rows = self.conn.query_sync(sql, params)?;
                if sql == inbox::RECIPIENT_PAGE_SQL && !self.fired.replace(true) {
                    self.cx.set_cancel_requested(true);
                }
                Ok(rows)
            }
            fn execute_raw(&self, sql: &str) -> Result<(), SqlError> {
                self.conn.execute_raw(sql)
            }
        }
        let (_directory, conn, _pool) = fixture();
        for caller_transaction in [false, true] {
            if caller_transaction {
                conn.execute_raw(
                    "BEGIN; UPDATE messages SET subject='pending caller write' WHERE id=10001",
                )
                .unwrap();
            } else {
                conn.execute_raw("PRAGMA query_only=ON").unwrap();
            }
            let cx = Cx::for_testing();
            let interrupted = Interrupt {
                conn: &conn,
                cx: &cx,
                fired: Cell::new(false),
            };
            assert!(matches!(
                read(&cx, &interrupted, &[12, 22], 100, true),
                Outcome::Cancelled(_)
            ));
            assert!(interrupted.fired.get());
            if caller_transaction {
                conn.execute_raw("ROLLBACK").unwrap();
            } else {
                assert_eq!(
                    conn.query_sync("PRAGMA query_only", &[]).unwrap()[0]
                        .get_as::<i64>(0)
                        .unwrap(),
                    1
                );
                conn.execute_raw("PRAGMA query_only=OFF").unwrap();
            }
            conn.execute_raw("BEGIN; ROLLBACK").unwrap();
            assert_eq!(
                conn.query_sync("SELECT subject FROM messages WHERE id=10001", &[])
                    .unwrap()[0]
                    .get_named::<String>("subject")
                    .unwrap(),
                "subject-10001"
            );
        }
    }

    #[test]
    fn missing_receipt_authority_is_an_error_not_a_partial_product_result() {
        let (_directory, conn, _pool) = fixture();
        conn.execute_raw("ALTER TABLE project_mailbox_receipts RENAME TO retained_receipts")
            .unwrap();
        let error = read(&Cx::for_testing(), &*conn, &[12, 22], 100, true)
            .into_result()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incomplete project mailbox schema")
        );
        conn.execute_raw("BEGIN; ROLLBACK").unwrap();
        assert!(
            !conn
                .query_sync("SELECT message_id FROM retained_receipts LIMIT 1", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_or_unwound_checkouts_cannot_recycle_uncommitted_transactions() {
        let (_directory, _conn, pool) = fixture();
        run(async {
            let cx = Cx::for_testing();
            for unwind in [false, true] {
                let connection = pool.acquire(&cx).await.into_result().unwrap();
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let checkout = ReadCheckout {
                        connection: Some(connection),
                        reusable: false,
                    };
                    checkout
                        .connection
                        .as_ref()
                        .unwrap()
                        .execute_raw(
                            "BEGIN; UPDATE messages SET subject='must roll back' WHERE id=10001",
                        )
                        .unwrap();
                    assert!(!unwind, "injected reader panic");
                }));
                assert_eq!(attempt.is_err(), unwind);
                let next = pool.acquire(&cx).await.into_result().unwrap();
                next.execute_raw("BEGIN; ROLLBACK").unwrap();
                assert_eq!(
                    next.query_sync("SELECT subject FROM messages WHERE id=10001", &[])
                        .unwrap()[0]
                        .get_named::<String>("subject")
                        .unwrap(),
                    "subject-10001"
                );
            }
            let cancelled_cx = Cx::for_testing();
            cancelled_cx.set_cancel_requested(true);
            assert!(matches!(
                fetch_inbox_for_product_agent_scoped(
                    &cancelled_cx,
                    &pool,
                    1,
                    "BlueLake",
                    &[12, 22],
                    false,
                    None,
                    7,
                    true
                )
                .await,
                Outcome::Cancelled(_)
            ));
        });
    }

    #[test]
    #[ignore = "explicit same-invocation product inbox scaling comparison"]
    fn product_window_does_not_scale_with_unrelated_message_history() {
        let (_directory, conn, pool) = fixture();
        run(async {
            let cx = Cx::for_testing();
            let mut medians = Vec::new();
            let mut previous = 0;
            for count in [1000, 10000] {
                conn.execute_raw("BEGIN").unwrap();
                for start in (previous + 1..=count).step_by(128) {
                    let end = (start + 127).min(count);
                    let messages = (start..=end)
                        .map(|n| {
                            format!("({},3,31,'noise','noise body',{})", 100_000 + n, n * 1000)
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    conn.execute_raw(&format!("INSERT INTO messages(id,project_id,sender_id,subject,body_md,created_ts) VALUES {messages}")).unwrap();
                    let recipients = (start..=end)
                        .map(|n| format!("({},32,'to')", 100_000 + n))
                        .collect::<Vec<_>>()
                        .join(",");
                    conn.execute_raw(&format!("INSERT INTO message_recipients(message_id,agent_id,kind) VALUES {recipients}")).unwrap();
                }
                conn.execute_raw("COMMIT").unwrap();
                previous = count;
                let mut candidate = Vec::new();
                let mut original = Vec::new();
                for round in 0..5 {
                    let new = async {
                        let start = Instant::now();
                        let rows = fetch_inbox_for_product_agent_scoped(
                            &cx,
                            &pool,
                            1,
                            "BlueLake",
                            &[12, 22],
                            false,
                            None,
                            20,
                            true,
                        )
                        .await
                        .into_result()
                        .unwrap();
                        (rows, start.elapsed().as_micros())
                    };
                    let old = async {
                        let start = Instant::now();
                        let rows = crate::queries::fetch_inbox_for_product_agent_scoped(
                            &cx,
                            &pool,
                            1,
                            "BlueLake",
                            &[12, 22],
                            false,
                            None,
                            20,
                            true,
                        )
                        .await
                        .into_result()
                        .unwrap();
                        (rows, start.elapsed().as_micros())
                    };
                    let (actual, expected) = if round % 2 == 0 {
                        let a = new.await;
                        (a, old.await)
                    } else {
                        let e = old.await;
                        (new.await, e)
                    };
                    assert_eq!(format!("{:?}", actual.0), format!("{:?}", expected.0));
                    candidate.push(actual.1);
                    original.push(expected.1);
                }
                candidate.sort_unstable();
                original.sort_unstable();
                println!(
                    "unrelated_messages={count} candidate_us={candidate:?} original_us={original:?}"
                );
                if count == 10000 {
                    assert!(
                        candidate[2] < original[2],
                        "candidate must beat the live fan-in"
                    );
                }
                medians.push(candidate[2]);
            }
            assert!(
                medians[1] <= 2 * medians[0],
                "unrelated history still dominates product reads: {medians:?}"
            );
        });
    }
}
