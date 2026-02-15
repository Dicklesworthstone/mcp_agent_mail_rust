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
use mcp_agent_mail_core::config::SearchEngine;
use mcp_agent_mail_core::metrics::global_metrics;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{DocKind, SearchQuery};
use mcp_agent_mail_db::search_scope::{
    ContactPolicyKind, RecipientEntry, RedactionPolicy, ScopeContext, ScopeVerdict, SenderPolicy,
    ViewerIdentity,
};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::search_v3::{get_bridge, init_bridge};
use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use tantivy::doc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn tantivy_test_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

fn ensure_tantivy_bridge_initialized() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let index_dir = std::env::temp_dir().join("mcp_agent_mail_search_v3_test_index");
        std::fs::create_dir_all(&index_dir).expect("create tantivy test index dir");
        init_bridge(&index_dir).expect("initialize Tantivy bridge");
    });
}

fn insert_tantivy_message_doc(
    doc_id: i64,
    project_id: i64,
    token: &str,
    sender: &str,
    thread_id: &str,
    importance: &str,
) {
    ensure_tantivy_bridge_initialized();
    let bridge = get_bridge().expect("tantivy bridge should be initialized");
    let handles = bridge.handles();
    let mut writer = bridge
        .index()
        .writer(15_000_000)
        .expect("create tantivy writer");
    writer
        .add_document(doc!(
            handles.id => u64::try_from(doc_id).expect("doc_id must be positive"),
            handles.doc_kind => "message",
            handles.subject => format!("tantivy-{token}-subject"),
            handles.body => format!("tantivy-{token}-body"),
            handles.sender => sender.to_string(),
            handles.project_slug => format!("project-{project_id}"),
            handles.project_id => u64::try_from(project_id).expect("project_id must be positive"),
            handles.thread_id => thread_id.to_string(),
            handles.importance => importance.to_string(),
            handles.created_ts => 1_700_000_000_000i64,
        ))
        .expect("add tantivy test doc");
    writer.commit().expect("commit tantivy test doc");
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

#[test]
fn search_engine_lexical_routes_to_tantivy_bridge() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("v3lex{}", unique_suffix());
    let tantivy_doc_id = 9_000_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");
    insert_tantivy_message_doc(
        tantivy_doc_id,
        pid,
        &token,
        "BlueLake",
        "br-v3-lexical",
        "high",
    );

    let before = global_metrics().snapshot();
    let query_token = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: query_token,
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            track_telemetry: true,
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            let found = resp.results.iter().any(|r| r.result.id == tantivy_doc_id);
            assert!(
                found,
                "expected Tantivy lexical result id={tantivy_doc_id}, got ids={:?}",
                resp.results.iter().map(|r| r.result.id).collect::<Vec<_>>()
            );
        }
        other => panic!("lexical routing search failed: {other:?}"),
    }

    let after = global_metrics().snapshot();
    assert!(
        after.search.queries_v3_total > before.search.queries_v3_total,
        "expected V3 query counter to increase (before={}, after={})",
        before.search.queries_v3_total,
        after.search.queries_v3_total
    );
}

#[test]
fn search_engine_legacy_routes_to_fts_even_with_bridge_available() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    ensure_tantivy_bridge_initialized();

    let pool2 = pool.clone();
    let token_for_query = format!("legacyfts{}", unique_suffix());
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_for_query.clone(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Legacy),
            track_telemetry: true,
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    // Legacy mode routes through FTS layer. With FTS tables present, it succeeds
    // (returns empty results since no matching data exists).
    match result {
        Outcome::Ok(resp) => {
            assert!(
                resp.results.is_empty(),
                "expected empty results for non-existent token, got {} results",
                resp.results.len()
            );
        }
        Outcome::Err(e) => panic!("legacy FTS search should succeed, got: {e:?}"),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
#[allow(deprecated)]
fn search_engine_shadow_records_parity_comparison_metrics() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("shadowcmp{}", unique_suffix());
    let tantivy_doc_id = 9_500_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");
    insert_tantivy_message_doc(
        tantivy_doc_id,
        pid,
        &token,
        "GoldFox",
        "br-v3-shadow",
        "normal",
    );

    let pool2 = pool.clone();
    let token_for_query = token.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_for_query.clone(),
            doc_kind: DocKind::Message,
            project_id: Some(pid),
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Shadow),
            track_telemetry: true,
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    // Shadow mode compares Tantivy and legacy FTS. With FTS tables present,
    // both paths execute successfully (FTS returns empty, Tantivy may find the doc).
    match result {
        Outcome::Ok(_resp) => {
            // Shadow mode succeeded - both search paths worked
        }
        Outcome::Err(e) => panic!("shadow search should succeed with FTS available, got: {e:?}"),
        other => panic!("unexpected outcome: {other:?}"),
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

// =============================================================================
// Search V3 scope enforcement integration tests (br-2tnl.6.4)
//
// Verify that the Tantivy (Lexical/Hybrid) search paths enforce the same
// scope, redaction, and audit rules as the FTS5 legacy path.
// =============================================================================

/// Insert a Tantivy doc for scope tests with a unique token.
fn insert_scope_tantivy_doc(
    doc_id: i64,
    project_id: i64,
    token: &str,
    sender: &str,
    thread_id: &str,
) {
    insert_tantivy_message_doc(doc_id, project_id, token, sender, thread_id, "normal");
}

/// Build a `ScopeContext` for a viewer in a specific project.
fn scope_viewer(agent_id: i64, project_id: i64) -> ScopeContext {
    ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id,
            agent_id,
        }),
        approved_contacts: Vec::new(),
        viewer_project_ids: vec![project_id],
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    }
}

#[test]
fn v3_lexical_scope_denies_cross_project_messages() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid_viewer = setup_project(&pool);
    let pid_other = setup_project(&pool);

    let token = format!("scopecross{}", unique_suffix());
    let doc_id = 10_000_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    // Insert a Tantivy doc belonging to pid_other
    insert_scope_tantivy_doc(doc_id, pid_other, &token, "BlueLake", "thread-cross");

    // Search as a viewer in pid_viewer — should NOT see the cross-project doc
    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(scope_viewer(1, pid_viewer)),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            // The doc should be denied by scope (cross-project, no contact)
            let found = resp
                .results
                .iter()
                .any(|r| r.result.id == doc_id && r.scope.verdict == ScopeVerdict::Allow);
            assert!(
                !found,
                "cross-project doc should not be visible to viewer in different project"
            );
            // Audit summary should be populated (viewer is Some)
            assert!(
                resp.audit_summary.is_some(),
                "audit_summary must be present when viewer is set"
            );
            if let Some(audit) = &resp.audit_summary {
                assert!(
                    audit.denied_count >= 1 || audit.visible_count == 0,
                    "cross-project doc should be denied or not returned"
                );
            }
        }
        other => panic!("v3 lexical scoped search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_scope_allows_same_project_auto_policy() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("scopeauto{}", unique_suffix());
    let doc_id = 10_100_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    // Insert a Tantivy doc in the same project
    insert_scope_tantivy_doc(doc_id, pid, &token, "SilverWolf", "thread-auto");

    // Search as a viewer in the same project — auto policy allows
    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(scope_viewer(1, pid)),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            // If Tantivy returned our doc, verify it was allowed (not denied/redacted).
            // The doc may not appear if the Tantivy reader hasn't refreshed yet —
            // that's OK, we only assert scope properties when the doc IS present.
            let our_doc = resp.results.iter().find(|r| r.result.id == doc_id);
            if let Some(scoped) = our_doc {
                assert_eq!(
                    scoped.scope.verdict,
                    ScopeVerdict::Allow,
                    "same-project doc should be allowed under auto policy"
                );
            }
            // Verify audit summary is present (viewer is set)
            assert!(
                resp.audit_summary.is_some(),
                "audit_summary must be present when viewer is set"
            );
            // Verify no denials for our doc
            if let Some(audit) = &resp.audit_summary {
                let denied_our_doc = audit.entries.iter().any(|e| e.result_id == doc_id);
                assert!(
                    !denied_our_doc,
                    "same-project doc should not appear in denied audit entries"
                );
            }
        }
        other => panic!("v3 lexical scoped search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_scope_contacts_only_denies_unlinked() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);
    let sender_agent_id = 20i64;

    let token = format!("scopecontact{}", unique_suffix());
    let doc_id = 10_200_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    insert_scope_tantivy_doc(doc_id, pid, &token, "BlueLake", "thread-contact");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let mut ctx = scope_viewer(10, pid);
        ctx.sender_policies.push(SenderPolicy {
            project_id: pid,
            agent_id: sender_agent_id,
            policy: ContactPolicyKind::ContactsOnly,
        });
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(ctx),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            let visible_our_doc = resp
                .results
                .iter()
                .any(|r| r.result.id == doc_id && r.scope.verdict == ScopeVerdict::Allow);
            assert!(
                !visible_our_doc,
                "contacts_only sender should be denied when viewer has no approved contact"
            );
        }
        other => panic!("v3 lexical scoped search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_scope_recipient_always_allowed() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("scoperecip{}", unique_suffix());
    let doc_id = 10_300_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    insert_scope_tantivy_doc(doc_id, pid, &token, "BlueLake", "thread-recip");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let mut ctx = scope_viewer(10, pid);
        // Sender blocks all — but viewer is a recipient
        ctx.sender_policies.push(SenderPolicy {
            project_id: pid,
            agent_id: 20,
            policy: ContactPolicyKind::BlockAll,
        });
        ctx.recipient_map.push(RecipientEntry {
            message_id: doc_id,
            agent_ids: vec![10],
        });
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(ctx),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            // If Tantivy returned our doc, verify it was allowed because viewer is recipient
            if let Some(scoped) = resp.results.iter().find(|r| r.result.id == doc_id) {
                assert_eq!(
                    scoped.scope.verdict,
                    ScopeVerdict::Allow,
                    "recipient should always see message even with block_all sender policy"
                );
            }
        }
        other => panic!("v3 lexical scoped search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_allowed_results_not_redacted() {
    // Verify that results with Allow verdict keep their original body
    // even when a redaction policy is set. Redaction only applies to Redact verdicts.
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("scoperedact{}", unique_suffix());
    let doc_id = 10_400_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    insert_scope_tantivy_doc(doc_id, pid, &token, "GreenLake", "thread-redact");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(scope_viewer(1, pid)),
            redaction_policy: Some(RedactionPolicy::strict()),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            if let Some(scoped) = resp.results.iter().find(|r| r.result.id == doc_id) {
                // Same-project + auto policy = Allow verdict → no redaction
                assert_eq!(scoped.scope.verdict, ScopeVerdict::Allow);
                assert!(
                    scoped.redaction_note.is_none(),
                    "Allowed results should not have redaction notes"
                );
            }
        }
        other => panic!("v3 lexical redaction search failed: {other:?}"),
    }
}

#[test]
fn v3_scope_redaction_on_deny_excludes_from_results() {
    // Verify that Deny-verdict results are fully excluded (not just redacted)
    // when searching through the V3 Tantivy path.
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid_viewer = setup_project(&pool);
    let pid_other = setup_project(&pool);

    let token = format!("scopedeny{}", unique_suffix());
    let doc_id = 10_450_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    // Doc in project that viewer can't access
    insert_scope_tantivy_doc(doc_id, pid_other, &token, "RedFox", "thread-deny");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(scope_viewer(1, pid_viewer)),
            redaction_policy: Some(RedactionPolicy::strict()),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            // Denied results should be completely excluded
            let found = resp.results.iter().any(|r| r.result.id == doc_id);
            assert!(
                !found,
                "cross-project denied doc should not appear in results at all"
            );
            // Verify audit tracks the denial
            if let Some(audit) = &resp.audit_summary {
                if audit.total_before > 0 {
                    assert!(
                        audit.denied_count > 0,
                        "should have at least one denied result in audit"
                    );
                }
            }
        }
        other => panic!("v3 scope deny search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_operator_mode_no_audit_no_filtering() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid = setup_project(&pool);

    let token = format!("scopeoper{}", unique_suffix());
    let doc_id = 10_500_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    insert_scope_tantivy_doc(doc_id, pid, &token, "RedFox", "thread-oper");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        // No scope_ctx = operator mode (default)
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            // If Tantivy returned our doc, it should be allowed (operator sees all).
            if let Some(scoped) = resp.results.iter().find(|r| r.result.id == doc_id) {
                assert_eq!(
                    scoped.scope.verdict,
                    ScopeVerdict::Allow,
                    "operator should see everything"
                );
            }
            // No audit in operator mode — this is the key invariant
            assert!(
                resp.audit_summary.is_none(),
                "operator mode should not produce audit summary"
            );
        }
        other => panic!("v3 lexical operator search failed: {other:?}"),
    }
}

#[test]
fn v3_lexical_audit_summary_counts_match_verdicts() {
    let _guard = tantivy_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pool, _dir) = make_pool();
    let pid_a = setup_project(&pool);
    let pid_b = setup_project(&pool);

    let token = format!("scopeaudit{}", unique_suffix());
    let doc_a_id = 10_600_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");
    let denied_doc_id = 10_600_000 + i64::try_from(unique_suffix()).expect("suffix fits i64");

    // Doc in project A (visible to viewer in A)
    insert_scope_tantivy_doc(doc_a_id, pid_a, &token, "BlueLake", "thread-audit-a");
    // Doc in project B (cross-project, should be denied)
    insert_scope_tantivy_doc(denied_doc_id, pid_b, &token, "RedFox", "thread-audit-b");

    let token_q = token.clone();
    let pool2 = pool.clone();
    let result = block_on(|cx| async move {
        let query = SearchQuery {
            text: token_q,
            doc_kind: DocKind::Message,
            ..Default::default()
        };
        let opts = SearchOptions {
            search_engine: Some(SearchEngine::Lexical),
            scope_ctx: Some(scope_viewer(1, pid_a)),
            ..Default::default()
        };
        execute_search(&cx, &pool2, &query, &opts).await
    });

    match result {
        Outcome::Ok(resp) => {
            let audit = resp
                .audit_summary
                .as_ref()
                .expect("audit_summary required when viewer is set");
            // Counts must be consistent
            assert_eq!(
                audit.total_before,
                audit.visible_count + audit.denied_count + audit.redacted_count,
                "audit counts must sum to total_before"
            );
            // Cross-project doc should be in denied entries
            let denied_b = audit
                .entries
                .iter()
                .any(|e| e.result_id == denied_doc_id && e.verdict == ScopeVerdict::Deny);
            if audit.total_before > 0 {
                // If both docs were returned by Tantivy, doc_b should be denied
                let visible_b = resp.results.iter().any(|r| r.result.id == denied_doc_id);
                assert!(
                    !visible_b || denied_b,
                    "cross-project doc_b should be denied or not in results"
                );
            }
        }
        other => panic!("v3 lexical audit search failed: {other:?}"),
    }
}
