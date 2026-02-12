//! Integration tests for `queries.rs` error paths and `search_service.rs` orchestration.
//!
//! These tests exercise the real DB layer (no mocks) to verify:
//! - Identity tool error paths (invalid name, duplicate, missing project)
//! - Messaging error paths (orphan reply, dupe ack, nonexistent message)
//! - Search service orchestration with real FTS
//! - File reservation and contact error paths

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::redundant_clone
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{DocKind, SearchQuery};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("query_integ_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

/// Helper: ensure a project and return its id.
fn setup_project(pool: &DbPool) -> i64 {
    let pool = pool.clone();
    let key = format!("/tmp/test_project_{}", unique_suffix());
    block_on(|cx| async move {
        match queries::ensure_project(&cx, &pool, &key).await {
            Outcome::Ok(p) => p.id.unwrap(),
            other => panic!("ensure_project failed: {other:?}"),
        }
    })
}

/// Helper: register an agent and return its id.
fn setup_agent(pool: &DbPool, project_id: i64, name: &str) -> i64 {
    let pool = pool.clone();
    let name = name.to_string();
    block_on(|cx| async move {
        match queries::register_agent(
            &cx,
            &pool,
            project_id,
            &name,
            "test",
            "test-model",
            Some("integration test"),
            None,
        )
        .await
        {
            Outcome::Ok(a) => a.id.unwrap(),
            other => panic!("register_agent({name}) failed: {other:?}"),
        }
    })
}

/// Helper: send a message with a recipient and return its id.
fn send_msg(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    recipient_id: i64,
    subject: &str,
    body: &str,
    thread_id: Option<&str>,
) -> i64 {
    let pool = pool.clone();
    let subject = subject.to_string();
    let body = body.to_string();
    let thread_id = thread_id.map(String::from);
    block_on(|cx| async move {
        let msg = match queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender_id,
            &subject,
            &body,
            thread_id.as_deref(),
            "normal",
            false,
            "[]",
            &[(recipient_id, "to")],
        )
        .await
        {
            Outcome::Ok(m) => m,
            other => panic!("create_message_with_recipients failed: {other:?}"),
        };
        msg.id.unwrap()
    })
}

// =============================================================================
// Identity error path tests (br-3h13.4.1)
// =============================================================================

#[test]
fn register_agent_invalid_name_rejected() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::register_agent(&cx, &pool2, pid, "EaglePeak", "test", "model", None, None).await
    });
    assert!(
        matches!(result, Outcome::Err(DbError::InvalidArgument { .. })),
        "expected InvalidArgument, got: {result:?}"
    );
}

#[test]
fn register_agent_empty_name_rejected() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::register_agent(&cx, &pool2, pid, "", "test", "model", None, None).await
    });
    assert!(
        matches!(result, Outcome::Err(_)),
        "expected error for empty name"
    );
}

#[test]
fn register_agent_idempotent_upsert() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let id1 = setup_agent(&pool, pid, "GoldFox");

    // Second registration with different program — should upsert
    let pool2 = pool.clone();
    let id2 = block_on(|cx| async move {
        match queries::register_agent(
            &cx,
            &pool2,
            pid,
            "GoldFox",
            "new-program",
            "new-model",
            Some("updated"),
            None,
        )
        .await
        {
            Outcome::Ok(a) => a.id.unwrap(),
            other => panic!("upsert failed: {other:?}"),
        }
    });
    assert_eq!(id1, id2, "upsert should return same agent id");
}

#[test]
fn create_agent_duplicate_name_rejected() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let _id1 = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::create_agent(
            &cx,
            &pool2,
            pid,
            "GoldFox",
            "test",
            "model",
            Some("dup test"),
            None,
        )
        .await
    });
    assert!(
        matches!(result, Outcome::Err(DbError::Duplicate { .. })),
        "expected Duplicate, got: {result:?}"
    );
}

#[test]
fn ensure_project_relative_path_rejected() {
    let (pool, _dir) = make_pool();
    let pool2 = pool.clone();
    let result =
        block_on(|cx| async move { queries::ensure_project(&cx, &pool2, "relative/path").await });
    assert!(
        matches!(result, Outcome::Err(DbError::InvalidArgument { .. })),
        "expected InvalidArgument, got: {result:?}"
    );
}

#[test]
fn get_agent_nonexistent_returns_not_found() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let pool2 = pool.clone();
    let result =
        block_on(|cx| async move { queries::get_agent(&cx, &pool2, pid, "PurpleDragon").await });
    assert!(
        matches!(result, Outcome::Err(DbError::NotFound { .. })),
        "expected NotFound, got: {result:?}"
    );
}

// =============================================================================
// Messaging error path tests (br-3h13.4.2)
// =============================================================================

#[test]
fn mark_read_nonexistent_message_no_crash() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");
    let pool2 = pool.clone();
    let result =
        block_on(
            |cx| async move { queries::mark_message_read(&cx, &pool2, 99999, agent_id).await },
        );
    // Should not crash — either Ok or NotFound
    match result {
        Outcome::Ok(_) | Outcome::Err(DbError::NotFound { .. }) => {}
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn acknowledge_nonexistent_message_no_crash() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");
    let pool2 = pool.clone();
    let result =
        block_on(
            |cx| async move { queries::acknowledge_message(&cx, &pool2, 99999, agent_id).await },
        );
    match result {
        Outcome::Ok(_) | Outcome::Err(DbError::NotFound { .. }) => {}
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn acknowledge_message_idempotent() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    let msg_id = send_msg(&pool, pid, sender_id, recip_id, "test", "body", None);

    // First ack (agent_id, message_id)
    let pool2 = pool.clone();
    block_on(|cx| async move {
        match queries::acknowledge_message(&cx, &pool2, recip_id, msg_id).await {
            Outcome::Ok(_) => {}
            other => panic!("first ack failed: {other:?}"),
        }
    });

    // Second ack — should succeed (idempotent)
    let pool3 = pool.clone();
    block_on(|cx| async move {
        match queries::acknowledge_message(&cx, &pool3, recip_id, msg_id).await {
            Outcome::Ok(_) => {}
            other => panic!("second ack failed: {other:?}"),
        }
    });
}

#[test]
fn fetch_inbox_for_nonexistent_agent_returns_empty() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::fetch_inbox(&cx, &pool2, pid, 99999, false, None, 20).await
    });
    match result {
        Outcome::Ok(rows) => assert!(rows.is_empty()),
        Outcome::Err(_) => {} // error is also acceptable
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn get_message_nonexistent_returns_not_found() {
    let (pool, _dir) = make_pool();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move { queries::get_message(&cx, &pool2, 99999).await });
    assert!(
        matches!(result, Outcome::Err(DbError::NotFound { .. })),
        "expected NotFound, got: {result:?}"
    );
}

#[test]
fn create_message_with_empty_recipients_succeeds() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::create_message_with_recipients(
            &cx,
            &pool2,
            pid,
            sender_id,
            "No recipients",
            "body",
            None,
            "normal",
            false,
            "[]",
            &[],
        )
        .await
    });
    assert!(
        matches!(result, Outcome::Ok(_)),
        "empty recipients should be ok"
    );
}

// =============================================================================
// Search service integration tests (br-3h13.2.1)
// =============================================================================

#[test]
fn search_empty_database_returns_no_results() {
    let (pool, _dir) = make_pool();
    let _pid = setup_project(&pool);

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "nonexistent".to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => assert!(resp.results.is_empty()),
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn search_finds_matching_message() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    send_msg(
        &pool,
        pid,
        sender_id,
        recip_id,
        "Build plan for API refactor",
        "We need to refactor the users endpoint for better performance",
        Some("PR-100"),
    );

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "refactor".to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => {
            assert!(
                !resp.results.is_empty(),
                "expected at least 1 result for 'refactor'"
            );
        }
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn search_prefix_wildcard() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    send_msg(
        &pool,
        pid,
        sender_id,
        recip_id,
        "Database migration plan",
        "We need to migrate the auth tables",
        Some("DB-1"),
    );

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "migrat*".to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => {
            assert!(
                !resp.results.is_empty(),
                "expected at least 1 result for prefix 'migrat*'"
            );
        }
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn search_empty_query_returns_empty() {
    let (pool, _dir) = make_pool();
    let _pid = setup_project(&pool);

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: String::new(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => assert!(resp.results.is_empty()),
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn search_with_explain_includes_metadata() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    send_msg(
        &pool,
        pid,
        sender_id,
        recip_id,
        "Explain test message",
        "This tests the explain feature",
        None,
    );

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "explain".to_string(),
            doc_kind: DocKind::Message,
            explain: true,
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => {
            assert!(resp.explain.is_some(), "explain should be present");
        }
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn search_scoped_with_telemetry() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    send_msg(
        &pool,
        pid,
        sender_id,
        recip_id,
        "Scoped search test",
        "Testing scoped search pipeline",
        None,
    );

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "scoped".to_string(),
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            track_telemetry: true,
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            assert!(
                !resp.results.is_empty(),
                "expected at least 1 scoped result"
            );
            // No viewer = no audit summary
            assert!(resp.audit_summary.is_none());
        }
        other => panic!("scoped search failed: {other:?}"),
    }
}

#[test]
fn search_pagination_cursor() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_id = setup_agent(&pool, pid, "GoldFox");
    let recip_id = setup_agent(&pool, pid, "SilverWolf");

    for i in 0..5 {
        send_msg(
            &pool,
            pid,
            sender_id,
            recip_id,
            &format!("Pagination test message {i}"),
            &format!("Body for pagination test {i}"),
            None,
        );
    }

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: "pagination".to_string(),
            doc_kind: DocKind::Message,
            limit: Some(2),
            ..Default::default()
        };
        execute_search_simple(&cx, &pool2, &query).await
    });

    match result {
        Outcome::Ok(resp) => {
            assert!(resp.results.len() <= 2, "should respect limit");
            if resp.results.len() == 2 {
                assert!(resp.next_cursor.is_some(), "expected pagination cursor");
            }
        }
        other => panic!("search failed: {other:?}"),
    }
}

// =============================================================================
// File reservation tests (br-3h13.4.4)
// =============================================================================

#[test]
fn reserve_and_release_roundtrip() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let granted = block_on(|cx| async move {
        match queries::create_file_reservations(
            &cx,
            &pool2,
            pid,
            agent_id,
            &["app/api/*.py"],
            3600,
            true,
            "test",
        )
        .await
        {
            Outcome::Ok(res) => res,
            other => panic!("reserve failed: {other:?}"),
        }
    });
    assert!(!granted.is_empty(), "should have granted at least 1");

    let pool3 = pool.clone();
    let released = block_on(|cx| async move {
        match queries::release_reservations(&cx, &pool3, pid, agent_id, None, None).await {
            Outcome::Ok(n) => n,
            other => panic!("release failed: {other:?}"),
        }
    });
    assert!(released > 0, "should have released at least 1");
}

#[test]
fn renew_no_active_reservations_returns_empty() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::renew_reservations(&cx, &pool2, pid, agent_id, 1800, None, None).await
    });
    match result {
        Outcome::Ok(renewed) => assert!(renewed.is_empty()),
        other => panic!("renew failed: {other:?}"),
    }
}

// =============================================================================
// Contact tests (br-3h13.4.3)
// =============================================================================

#[test]
fn list_contacts_empty_returns_empty_tuples() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let result =
        block_on(|cx| async move { queries::list_contacts(&cx, &pool2, pid, agent_id).await });
    match result {
        Outcome::Ok((outgoing, incoming)) => {
            assert!(outgoing.is_empty());
            assert!(incoming.is_empty());
        }
        other => panic!("list_contacts failed: {other:?}"),
    }
}

#[test]
fn request_contact_and_respond_accept() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let fox_id = setup_agent(&pool, pid, "GoldFox");
    let wolf_id = setup_agent(&pool, pid, "SilverWolf");

    // Request contact
    let pool2 = pool.clone();
    block_on(|cx| async move {
        match queries::request_contact(
            &cx,
            &pool2,
            pid,
            fox_id,
            pid,
            wolf_id,
            "want to chat",
            86400,
        )
        .await
        {
            Outcome::Ok(_) => {}
            other => panic!("request_contact failed: {other:?}"),
        }
    });

    // Accept
    let pool3 = pool.clone();
    block_on(|cx| async move {
        match queries::respond_contact(&cx, &pool3, pid, fox_id, pid, wolf_id, true, 2_592_000)
            .await
        {
            Outcome::Ok(_) => {}
            other => panic!("respond_contact failed: {other:?}"),
        }
    });

    // Verify allowed
    let pool4 = pool.clone();
    let allowed = block_on(|cx| async move {
        match queries::is_contact_allowed(&cx, &pool4, pid, fox_id, pid, wolf_id).await {
            Outcome::Ok(v) => v,
            other => panic!("is_contact_allowed failed: {other:?}"),
        }
    });
    assert!(allowed, "contact should be allowed after acceptance");
}

#[test]
fn set_contact_policy_contacts_only() {
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let agent_id = setup_agent(&pool, pid, "GoldFox");

    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        queries::set_agent_contact_policy(&cx, &pool2, agent_id, "contacts_only").await
    });
    assert!(
        matches!(result, Outcome::Ok(_)),
        "set_contact_policy should succeed"
    );
}
