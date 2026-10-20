//! Real mailbox controls for the explorer's snapshot and late-body pipeline.
//! These complement, rather than replace, the retained filter/privacy controls.
use super::*;
use asupersync::runtime::{Runtime, RuntimeBuilder};
use std::path::Path;

fn ok<T>(outcome: Outcome<T, impl std::fmt::Debug>) -> T {
    match outcome {
        Outcome::Ok(value) => value,
        other => panic!(
            "mailbox operation failed: {:?}",
            match other {
                Outcome::Err(error) => format!("{error:?}"),
                Outcome::Cancelled(_) => "cancelled".into(),
                Outcome::Panicked(_) => "panicked".into(),
                Outcome::Ok(_) => unreachable!(),
            }
        ),
    }
}

fn execute(runtime: &Runtime, pool: &DbPool, sql: &str) {
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime context");
        let conn = ok(pool.acquire(&cx).await);
        conn.execute_raw(sql).unwrap();
    });
}

fn fixture(test: impl FnOnce(&Runtime, &DbPool, &Path)) {
    mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("explorer.sqlite3");
        let pool = crate::create_pool(&crate::DbPoolConfig {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&path),
            storage_root: Some(directory.path().join("archive")),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .unwrap();
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        for sql in [
            "INSERT INTO projects(id,slug,human_key,created_at) VALUES(1,'alpha','/alpha',1),(2,'beta','/beta',1)",
            "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES(10,1,'AmberFox','test','test',1,1),(11,1,'BlueLake','test','test',1,1),(12,1,'HiddenFox','test','test',1,1),(20,2,'ZebraFox','test','test',1,1),(21,2,'BlueLake','test','test',1,1)",
            "INSERT INTO messages(id,project_id,sender_id,subject,body_md,thread_id,importance,ack_required,created_ts,recipients_json,attachments) VALUES(100,1,10,'shared 100%_done','body-100','alpha-thread','high',1,100,'{}','[]'),(101,1,10,'direct','body-101',NULL,'normal',0,200,'{}','[]'),(102,1,10,'both','body-102','alpha-thread','low',1,300,'{}','[]'),(200,2,20,'beta shared','body-200','beta-thread','urgent',1,150,'{}','[]'),(103,1,11,'self','body-103','self-thread','normal',0,250,'{}','[]')",
            "INSERT INTO message_recipients(message_id,agent_id,kind) VALUES(100,12,'bcc'),(101,11,'to'),(102,11,'bcc'),(103,11,'to')",
            "INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts) VALUES(100,1,'to',100),(102,1,'cc',300),(200,2,'to',150)",
            "INSERT INTO project_mailbox_receipts(message_id,agent_id,read_ts,ack_ts) VALUES(100,11,7,8),(102,11,99,99),(200,11,88,88)",
        ] {
            execute(&runtime, &pool, sql);
        }
        test(&runtime, &pool, &path);
    });
}

fn query() -> ExplorerQuery {
    ExplorerQuery {
        agent_name: "BlueLake".into(),
        direction: Direction::Inbound,
        limit: 50,
        ..Default::default()
    }
}

fn page(runtime: &Runtime, pool: &DbPool, query: &ExplorerQuery) -> ExplorerPage {
    runtime.block_on(async {
        let cx = Cx::current().expect("runtime context");
        ok(fetch_explorer_page(&cx, pool, query).await)
    })
}

fn entry_values(entries: &[ExplorerEntry]) -> serde_json::Value {
    serde_json::to_value(entries).unwrap()
}

#[test]
fn actual_candidates_never_return_message_bodies() {
    fixture(|runtime, pool, _| {
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            let conn = ok(pool.acquire(&cx).await);
            for sort in [
                SortMode::DateDesc,
                SortMode::DateAsc,
                SortMode::ImportanceDesc,
                SortMode::AgentAlpha,
            ] {
                for inbound in [true, false] {
                    let query = ExplorerQuery { sort, ..query() };
                    let (source, conditions, params) = lane_query(1, 11, &query, true, inbound);
                    let sql = candidate_sql(&source, &conditions, &query, true, inbound);
                    let rows = conn.query_sync(&sql, &params).unwrap();
                    assert!(!rows.is_empty());
                    for row in rows {
                        assert_eq!(row.get_as::<String>(5).unwrap(), "");
                    }
                }
            }
        });
    });
}

#[test]
fn all_modes_and_filters_keep_the_same_page_as_the_complete_query() {
    fixture(|runtime, pool, _| {
        for direction in [Direction::Inbound, Direction::Outbound, Direction::All] {
            for sort in [
                SortMode::DateDesc,
                SortMode::DateAsc,
                SortMode::ImportanceDesc,
                SortMode::AgentAlpha,
            ] {
                for ack_filter in [
                    AckFilter::All,
                    AckFilter::Unread,
                    AckFilter::PendingAck,
                    AckFilter::Acknowledged,
                ] {
                    for importance_filter in [vec![], vec!["high".into(), "urgent".into()]] {
                        let base = ExplorerQuery {
                            direction,
                            sort,
                            ack_filter,
                            importance_filter,
                            limit: 100,
                            ..query()
                        };
                        let complete = page(runtime, pool, &base);
                        for offset in [0, 1, 3, 20] {
                            for limit in [0, 1, 2, 7] {
                                let window = page(
                                    runtime,
                                    pool,
                                    &ExplorerQuery {
                                        limit,
                                        offset,
                                        ..base.clone()
                                    },
                                );
                                let expected: Vec<_> = complete
                                    .entries
                                    .iter()
                                    .skip(offset)
                                    .take(limit)
                                    .cloned()
                                    .collect();
                                assert_eq!(entry_values(&window.entries), entry_values(&expected));
                                assert_eq!(window.total_count, complete.total_count);
                                assert_eq!(window.stats.unread_count, complete.stats.unread_count);
                                assert_eq!(
                                    window.stats.pending_ack_count,
                                    complete.stats.pending_ack_count
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn actionable_counts_include_unreturned_direct_and_shared_deliveries() {
    fixture(|runtime, pool, _| {
        for (limit, offset) in [(0, 0), (1, 0), (2, 2), (1, 100)] {
            let result = page(
                runtime,
                pool,
                &ExplorerQuery {
                    limit,
                    offset,
                    ..query()
                },
            );
            assert_eq!(result.total_count, 5);
            assert_eq!(result.stats.inbound_count, 5);
            assert_eq!(result.stats.outbound_count, 0);
            assert_eq!(result.stats.unread_count, 4);
            assert_eq!(result.stats.pending_ack_count, 2);
        }
        let pending = page(
            runtime,
            pool,
            &ExplorerQuery {
                ack_filter: AckFilter::PendingAck,
                limit: 1,
                ..query()
            },
        );
        assert_eq!(pending.total_count, 2);
        assert_eq!(pending.stats.pending_ack_count, 2);
        assert_eq!(pending.stats.unread_count, 2);
    });
}

#[test]
fn discarded_body_does_not_poison_a_valid_global_page() {
    fixture(|runtime, pool, _| {
        // This is a candidate in beta's lane but loses the global newest page.
        execute(
            runtime,
            pool,
            "UPDATE messages SET body_md = X'FFFE' WHERE id = 200",
        );
        let result = page(
            runtime,
            pool,
            &ExplorerQuery {
                limit: 1,
                ..query()
            },
        );
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].message_id, 102);
        assert_eq!(result.entries[0].body_md, "body-102");
        // It is not silently accepted when the caller actually selects it.
        let failed = runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            fetch_explorer_page(
                &cx,
                pool,
                &ExplorerQuery {
                    project_id: Some(2),
                    limit: 1,
                    ..query()
                },
            )
            .await
        });
        assert!(matches!(failed, Outcome::Err(_)));
        execute(
            runtime,
            pool,
            "UPDATE messages SET body_md = 'repaired' WHERE id = 200",
        );
        let restored = page(
            runtime,
            pool,
            &ExplorerQuery {
                project_id: Some(2),
                limit: 1,
                ..query()
            },
        );
        assert_eq!(restored.entries[0].body_md, "repaired");
    });
}

#[test]
fn zero_limit_only_counts_even_when_no_body_is_decodable() {
    fixture(|runtime, pool, _| {
        execute(runtime, pool, "UPDATE messages SET body_md = X'FFFE'");
        let result = page(
            runtime,
            pool,
            &ExplorerQuery {
                limit: 0,
                offset: usize::MAX,
                ..query()
            },
        );
        assert!(result.entries.is_empty());
        assert_eq!(result.total_count, 5);
        assert_eq!(result.stats.unread_count, 4);
        assert_eq!(result.stats.pending_ack_count, 2);
    });
}

#[test]
fn body_receipts_and_counts_stay_on_one_snapshot_while_another_connection_writes() {
    fixture(|runtime, pool, path| {
        let writer = crate::guard_db_conn(
            DbConn::open_file(path.to_str().unwrap()).unwrap(),
            "explorer snapshot writer",
        );
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            let connection = ok(pool.acquire(&cx).await);
            let mut checkout = ReadCheckout { connection: Some(connection), reusable: false };
            let conn = &**checkout.connection.as_ref().unwrap();
            conn.execute_raw(READ_SAVEPOINT).unwrap();
            // Fix the actual native read snapshot before the other connection commits.
            assert_eq!(conn.query_sync("SELECT body_md FROM messages WHERE id=102", &[]).unwrap()[0].get_named::<String>("body_md").unwrap(), "body-102");
            writer.execute_raw("BEGIN IMMEDIATE; UPDATE messages SET body_md='changed' WHERE id=102; UPDATE message_recipients SET read_ts=44,ack_ts=45 WHERE message_id=102 AND agent_id=11; COMMIT").unwrap();
            let retained = ok(fetch_page_in_snapshot(&cx, conn, &ExplorerQuery { limit: 1, ..query() }).await);
            assert_eq!(retained.entries[0].body_md, "body-102");
            assert_eq!(retained.entries[0].ack_ts, None);
            assert_eq!(retained.stats.pending_ack_count, 2);
            conn.execute_raw(RELEASE_READ_SAVEPOINT).unwrap();
            checkout.reusable = true;
            drop(checkout);
            let fresh = ok(fetch_explorer_page(&cx, pool, &ExplorerQuery { limit: 1, ..query() }).await);
            assert_eq!(fresh.entries[0].body_md, "changed");
            assert_eq!(fresh.entries[0].ack_ts, Some(45));
            assert_eq!(fresh.stats.pending_ack_count, 1);
        });
    });
}

#[test]
fn query_only_exploration_preserves_private_receipts_and_bcc_policy() {
    fixture(|runtime, pool, _| {
        execute(runtime, pool, "PRAGMA query_only = ON");
        let result = page(runtime, pool, &query());
        assert!(
            result
                .entries
                .iter()
                .all(|entry| !entry.to_agents.contains("HiddenFox"))
        );
        assert_eq!(
            result
                .entries
                .iter()
                .find(|entry| entry.message_id == 102)
                .unwrap()
                .ack_ts,
            None
        );
        assert_eq!(
            result
                .entries
                .iter()
                .find(|entry| entry.message_id == 200)
                .unwrap()
                .read_ts,
            None
        );
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime context");
            let conn = ok(pool.acquire(&cx).await);
            let rows = conn
                .query_sync("SELECT COUNT(*) AS cnt FROM project_mailbox_receipts", &[])
                .unwrap();
            assert_eq!(map_count_rows(&rows).unwrap(), 3);
        });
    });
}

#[test]
fn expired_request_returns_cancellation_instead_of_a_page() {
    fixture(|runtime, pool, _| {
        let cx = Cx::for_request_with_budget(
            asupersync::Budget::INFINITE.with_deadline(asupersync::Time::ZERO),
        );
        let result = runtime.block_on(fetch_explorer_page(&cx, pool, &query()));
        assert!(matches!(result, Outcome::Cancelled(_)));
        assert_eq!(page(runtime, pool, &query()).total_count, 5);
    });
}
