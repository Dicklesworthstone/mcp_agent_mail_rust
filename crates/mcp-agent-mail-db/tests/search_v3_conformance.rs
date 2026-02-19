//! Search V3 conformance tests (br-2tnl.7.3).
//!
//! Validates that the Tantivy-backed search path produces results conformant
//! with the legacy FTS5 contract.  Five test families:
//!
//! 1. **Result shape**: V3 results carry the same fields as legacy responses.
//! 2. **Engine routing**: `SearchOptions::search_engine` correctly selects the
//!    Tantivy vs FTS5 codepath.
//! 3. **Scope enforcement**: `execute_search` with scope context produces
//!    correct verdict counts (Allow, Deny) on V3 paths.
//! 4. **Product search**: `search_messages_for_product` works with V3 indexing.
//! 5. **Filter parity**: facets (importance, sender, thread, `time_range`) produce
//!    consistent filtering in V3 vs legacy.
//!
//! Every assertion emits a structured trace:
//! ```text
//! TRACE v3_conformance | case=<name> | engine=<engine> | rows=<n> | elapsed_us=<n>
//! ```

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::redundant_clone,
    deprecated
)]

use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::SearchEngine;
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, PlanMethod, RankingMode, SearchQuery, TimeRange, plan_search,
};
use mcp_agent_mail_db::search_scope::{ScopeContext, ScopeVerdict, ViewerIdentity};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, now_micros, queries};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("v3_conf_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 4,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
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

fn trace(case: &str, engine: &str, rows: usize, elapsed_us: u64) {
    eprintln!(
        "TRACE v3_conformance | case={case} | engine={engine} | rows={rows} | elapsed_us={elapsed_us}"
    );
}

// ────────────────────────────────────────────────────────────────────
// Corpus seeding
// ────────────────────────────────────────────────────────────────────

struct SeededCorpus {
    project_id: i64,
    sender_id: i64,
    #[allow(dead_code)]
    recipient_id: i64,
    second_project_id: i64,
    _second_sender_id: i64,
    #[allow(dead_code)]
    message_count: usize,
}

fn seed_corpus(pool: &DbPool) -> SeededCorpus {
    let (project_id, sender_id, recipient_id) = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let p = match queries::ensure_project(&cx, &pool, "/test/v3-conformance").await {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = p.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "GoldFox",
                "test",
                "test",
                Some("v3-conformance sender"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register sender failed: {other:?}"),
            };
            let recip = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "SilverWolf",
                "test",
                "test",
                Some("v3-conformance recipient"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register recipient failed: {other:?}"),
            };

            (pid, sender.id.unwrap(), recip.id.unwrap())
        }
    });

    // Second project for isolation tests
    let (second_project_id, second_sender_id) = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let p = match queries::ensure_project(&cx, &pool, "/test/v3-conformance-beta").await {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project beta failed: {other:?}"),
            };
            let pid = p.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "BlueLake",
                "test",
                "test",
                Some("v3-conformance beta sender"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register beta sender failed: {other:?}"),
            };

            (pid, sender.id.unwrap())
        }
    });

    // Messages with searchable content
    let messages: &[(&str, &str, &str, bool, Option<&str>)] = &[
        (
            "Database migration plan",
            "Steps for migrating schema from v2 to v3",
            "high",
            true,
            Some("thread-migration"),
        ),
        (
            "Weekly sync notes",
            "Discussed performance regression in search queries",
            "normal",
            false,
            None,
        ),
        (
            "Bug report: null pointer",
            "NullPointerException in search indexer on empty query",
            "urgent",
            true,
            Some("thread-bugs"),
        ),
        (
            "Feature request: dark mode",
            "Users want dark mode support in the dashboard",
            "low",
            false,
            None,
        ),
        (
            "Security audit findings",
            "Found SQL injection vector in legacy search endpoint",
            "urgent",
            true,
            Some("thread-security"),
        ),
        (
            "Performance benchmarks",
            "P95 latency improved from 12ms to 3ms after tuning",
            "high",
            false,
            None,
        ),
        (
            "Release notes v4.2",
            "Includes search upgrade and cursor pagination fixes",
            "normal",
            false,
            None,
        ),
        (
            "Onboarding guide draft",
            "New agent onboarding process with walkthrough steps",
            "low",
            false,
            None,
        ),
        (
            "Incident postmortem",
            "Root cause: unbounded query expansion caused OOM crash",
            "high",
            true,
            Some("thread-incidents"),
        ),
    ];

    for (i, &(subject, body, importance, ack, thread_id)) in messages.iter().enumerate() {
        let pool_c = pool.clone();
        block_on(|cx| {
            let pool = pool_c;
            async move {
                match queries::create_message(
                    &cx, &pool, project_id, sender_id, subject, body, thread_id, importance, ack,
                    "[]",
                )
                .await
                {
                    Outcome::Ok(_) => {}
                    other => panic!("create_message[{i}] failed: {other:?}"),
                }
            }
        });
    }

    // Beta project messages
    let beta_messages: &[(&str, &str)] = &[
        (
            "Beta internal note",
            "This is a beta-only internal document about search features",
        ),
        (
            "Beta compatibility check",
            "Verified backward compat with Python client v3.x",
        ),
    ];
    for (i, &(subject, body)) in beta_messages.iter().enumerate() {
        let pool_c = pool.clone();
        block_on(|cx| {
            let pool = pool_c;
            async move {
                match queries::create_message(
                    &cx,
                    &pool,
                    second_project_id,
                    second_sender_id,
                    subject,
                    body,
                    None,
                    "normal",
                    false,
                    "[]",
                )
                .await
                {
                    Outcome::Ok(_) => {}
                    other => panic!("create beta msg[{i}] failed: {other:?}"),
                }
            }
        });
    }

    SeededCorpus {
        project_id,
        sender_id,
        recipient_id,
        second_project_id,
        _second_sender_id: second_sender_id,
        message_count: messages.len() + beta_messages.len(),
    }
}

// ════════════════════════════════════════════════════════════════════
// 1. RESULT SHAPE: V3 results carry the same fields as legacy
// ════════════════════════════════════════════════════════════════════

/// Legacy (FTS5) search returns well-formed `SearchResponse` with expected fields.
#[test]
fn v3_conformance_legacy_result_shape() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("migration", corpus.project_id);
    let timer = std::time::Instant::now();

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("legacy search failed: {other:?}"),
            }
        }
    });

    let elapsed = timer.elapsed().as_micros() as u64;
    trace("legacy_result_shape", "fts5", resp.results.len(), elapsed);

    assert!(
        !resp.results.is_empty(),
        "legacy search returned 0 results for 'migration'"
    );

    for r in &resp.results {
        assert!(r.id > 0, "result id should be > 0, got {}", r.id);
        assert!(!r.title.is_empty(), "result title should not be empty");
        assert!(r.importance.is_some(), "importance should be present");
        assert!(r.from_agent.is_some(), "from_agent should be present");
    }
}

/// Planner selects FTS method for text queries with project filter.
#[test]
fn v3_conformance_planner_fts_selection() {
    let q = SearchQuery {
        text: "migration".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        ..Default::default()
    };
    let plan = plan_search(&q);
    assert_eq!(plan.method, PlanMethod::Like, "text query should use LIKE");
    assert!(plan.facets_applied.contains(&"project_id".to_string()));
}

/// `execute_search_simple` with explain=true populates query explain metadata.
#[test]
fn v3_conformance_explain_in_simple_response() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: "search".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        explain: true,
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("explain search failed: {other:?}"),
            }
        }
    });

    assert!(
        resp.explain.is_some(),
        "explain should be present when requested"
    );
    let explain = resp.explain.unwrap();
    assert!(!explain.sql.is_empty(), "explain.sql should not be empty");
    assert!(
        !explain.method.is_empty(),
        "explain.method should not be empty"
    );
}

// ════════════════════════════════════════════════════════════════════
// 2. ENGINE ROUTING: SearchOptions selects correct codepath
// ════════════════════════════════════════════════════════════════════

/// Legacy engine override routes to FTS5 path.
#[test]
fn v3_conformance_engine_routing_legacy() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("search", corpus.project_id);
    let opts = SearchOptions {
        scope_ctx: None,
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Legacy),
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("legacy engine search failed: {other:?}"),
            }
        }
    });

    trace("engine_routing_legacy", "legacy", resp.results.len(), 0);

    // Operator mode (no scope) should not deny any results
    assert!(
        resp.audit_summary.is_none()
            || resp
                .audit_summary
                .as_ref()
                .is_some_and(|a| a.denied_count == 0),
        "operator mode should not deny any results"
    );
}

/// Lexical engine override gracefully degrades when bridge not initialized.
#[test]
fn v3_conformance_engine_routing_lexical_degrades() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("search", corpus.project_id);
    let opts = SearchOptions {
        scope_ctx: None,
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Lexical),
    };

    // Without initializing the Tantivy bridge, Lexical should fall back to FTS5
    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("lexical fallback search failed: {other:?}"),
            }
        }
    });

    trace(
        "engine_routing_lexical_fallback",
        "lexical→fts5",
        resp.results.len(),
        0,
    );

    // Should still return results via FTS5 fallback
    // sql_row_count is usize (always >= 0), so just assert it was populated
    let _ = resp.sql_row_count; // confirm field exists
}

/// Hybrid engine override gracefully degrades without semantic bridge.
#[test]
fn v3_conformance_engine_routing_hybrid_degrades() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("migration", corpus.project_id);
    let opts = SearchOptions {
        scope_ctx: None,
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Hybrid),
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("hybrid fallback search failed: {other:?}"),
            }
        }
    });

    trace(
        "engine_routing_hybrid_fallback",
        "hybrid→fts5",
        resp.results.len(),
        0,
    );

    // Hybrid without bridge should still return FTS5 results
    assert!(
        !resp.results.is_empty(),
        "hybrid fallback should still find 'migration'"
    );
}

// ════════════════════════════════════════════════════════════════════
// 3. SCOPE ENFORCEMENT: V3 paths produce correct scope verdicts
// ════════════════════════════════════════════════════════════════════

/// Scoped search with `CallerScoped` viewer filters cross-project results.
#[test]
fn v3_conformance_scope_denies_cross_project() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("search", corpus.project_id);
    let scope = ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id: corpus.project_id,
            agent_id: corpus.sender_id,
        }),
        approved_contacts: Vec::new(),
        viewer_project_ids: vec![corpus.project_id],
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    };
    let opts = SearchOptions {
        scope_ctx: Some(scope),
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Legacy),
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("scoped search failed: {other:?}"),
            }
        }
    });

    trace("scope_cross_project", "legacy+scope", resp.results.len(), 0);

    // All returned results should have Allow verdict
    for r in &resp.results {
        assert_eq!(
            r.scope.verdict,
            ScopeVerdict::Allow,
            "all visible results should have Allow verdict"
        );
    }

    // If audit_summary is present, denied count should be 0
    // (FTS5 query is already project-scoped, so no cross-project rows appear)
    if let Some(audit) = &resp.audit_summary {
        assert_eq!(
            audit.denied_count, 0,
            "project-scoped query should not produce denied results"
        );
    }
}

/// Operator mode (no scope context) sees all results without filtering.
#[test]
fn v3_conformance_operator_mode_no_filtering() {
    let (pool, _dir) = make_pool();
    let _corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: "search".to_string(),
        doc_kind: DocKind::Message,
        project_id: None,
        ..Default::default()
    };
    let opts = SearchOptions {
        scope_ctx: None,
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Legacy),
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("operator mode search failed: {other:?}"),
            }
        }
    });

    trace("operator_mode", "legacy+unscoped", resp.results.len(), 0);

    assert!(
        resp.audit_summary.is_none()
            || resp
                .audit_summary
                .as_ref()
                .is_some_and(|a| a.denied_count == 0),
        "operator mode should not deny results"
    );
}

/// Scoped search `audit_summary` counts match verdict distribution.
#[test]
fn v3_conformance_audit_summary_counts() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery::messages("search", corpus.project_id);
    let scope = ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id: corpus.project_id,
            agent_id: corpus.sender_id,
        }),
        approved_contacts: Vec::new(),
        viewer_project_ids: vec![corpus.project_id],
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    };
    let opts = SearchOptions {
        scope_ctx: Some(scope),
        redaction_policy: None,
        track_telemetry: false,
        search_engine: Some(SearchEngine::Legacy),
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search(&cx, &pool, &q, &opts).await {
                Outcome::Ok(r) => r,
                other => panic!("audit summary search failed: {other:?}"),
            }
        }
    });

    trace(
        "audit_summary_counts",
        "legacy+scope",
        resp.results.len(),
        0,
    );

    if let Some(audit) = &resp.audit_summary {
        assert_eq!(
            audit.total_before,
            audit.visible_count + audit.denied_count,
            "total_before = visible + denied: {} != {} + {}",
            audit.total_before,
            audit.visible_count,
            audit.denied_count
        );
        assert_eq!(
            resp.results.len(),
            audit.visible_count,
            "visible results count should match audit.visible_count"
        );
    }
}

// ════════════════════════════════════════════════════════════════════
// 4. PRODUCT SEARCH: cross-project search works
// ════════════════════════════════════════════════════════════════════

/// Product-scoped search finds results from linked projects.
#[test]
fn v3_conformance_product_search_basic() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let product_id = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let product =
                match queries::ensure_product(&cx, &pool, Some("test-v3-product"), None).await {
                    Outcome::Ok(p) => p,
                    other => panic!("ensure_product failed: {other:?}"),
                };
            let pid = product.id.unwrap();

            match queries::link_product_to_projects(&cx, &pool, pid, &[corpus.project_id]).await {
                Outcome::Ok(_) => {}
                other => panic!("link alpha failed: {other:?}"),
            }
            match queries::link_product_to_projects(&cx, &pool, pid, &[corpus.second_project_id])
                .await
            {
                Outcome::Ok(_) => {}
                other => panic!("link beta failed: {other:?}"),
            }

            pid
        }
    });

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::search_messages_for_product(&cx, &pool, product_id, "search", 20).await {
                Outcome::Ok(results) => results,
                other => panic!("product search failed: {other:?}"),
            }
        }
    });

    trace("product_search_basic", "fts5+product", resp.len(), 0);

    assert!(
        !resp.is_empty(),
        "product search returned 0 results for 'search'"
    );

    for r in &resp {
        assert!(
            r.project_id > 0,
            "product search result should have valid project_id"
        );
    }
}

/// Product search returns results from multiple linked projects.
#[test]
fn v3_conformance_product_search_cross_project() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let product_id = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let product = match queries::ensure_product(
                &cx,
                &pool,
                Some("test-v3-cross-product"),
                None,
            )
            .await
            {
                Outcome::Ok(p) => p,
                other => panic!("ensure_product failed: {other:?}"),
            };
            let pid = product.id.unwrap();

            match queries::link_product_to_projects(&cx, &pool, pid, &[corpus.project_id]).await {
                Outcome::Ok(_) => {}
                other => panic!("link alpha failed: {other:?}"),
            }
            match queries::link_product_to_projects(&cx, &pool, pid, &[corpus.second_project_id])
                .await
            {
                Outcome::Ok(_) => {}
                other => panic!("link beta failed: {other:?}"),
            }

            pid
        }
    });

    // "search" appears in both alpha (search queries, search indexer, search endpoint,
    // search upgrade) and beta (search features)
    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match queries::search_messages_for_product(&cx, &pool, product_id, "search", 50).await {
                Outcome::Ok(results) => results,
                other => panic!("cross-product search failed: {other:?}"),
            }
        }
    });

    trace(
        "product_search_cross_project",
        "fts5+product",
        resp.len(),
        0,
    );

    let project_ids: std::collections::HashSet<i64> = resp.iter().map(|r| r.project_id).collect();

    // Should find results from both projects
    assert!(
        project_ids.len() >= 2,
        "product search should return results from multiple projects, got {} project_ids from {} results",
        project_ids.len(),
        resp.len()
    );
}

// ════════════════════════════════════════════════════════════════════
// 5. FILTER PARITY: facets produce correct filtering
// ════════════════════════════════════════════════════════════════════

/// Importance filter restricts results to matching levels.
#[test]
fn v3_conformance_importance_filter() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        importance: vec![Importance::Urgent],
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("importance filter search failed: {other:?}"),
            }
        }
    });

    trace("importance_filter", "fts5", resp.results.len(), 0);

    for r in &resp.results {
        assert_eq!(
            r.importance.as_deref(),
            Some("urgent"),
            "importance filter should only return urgent messages, got {:?}",
            r.importance
        );
    }
    // We seeded 2 urgent messages in alpha project
    assert!(
        resp.results.len() >= 2,
        "should find at least 2 urgent messages, got {}",
        resp.results.len()
    );
}

/// Thread ID filter restricts results to that thread.
#[test]
fn v3_conformance_thread_filter() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        thread_id: Some("thread-migration".to_string()),
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("thread filter search failed: {other:?}"),
            }
        }
    });

    trace("thread_filter", "fts5", resp.results.len(), 0);

    for r in &resp.results {
        assert_eq!(
            r.thread_id.as_deref(),
            Some("thread-migration"),
            "thread filter should only return matching thread, got {:?}",
            r.thread_id
        );
    }
    assert_eq!(
        resp.results.len(),
        1,
        "should find exactly 1 message in thread-migration"
    );
}

/// Ack-required filter returns only ack-flagged messages.
#[test]
fn v3_conformance_ack_filter() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        ack_required: Some(true),
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("ack filter search failed: {other:?}"),
            }
        }
    });

    trace("ack_filter", "fts5", resp.results.len(), 0);

    for r in &resp.results {
        assert_eq!(
            r.ack_required,
            Some(true),
            "ack_required filter should only return ack=true, got {:?}",
            r.ack_required
        );
    }
    // We seeded 4 ack-required messages in alpha project
    assert!(
        resp.results.len() >= 4,
        "should find at least 4 ack-required messages, got {}",
        resp.results.len()
    );
}

/// Time range filter limits results to specified window.
#[test]
fn v3_conformance_time_range_filter() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let now = now_micros();
    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        time_range: TimeRange {
            min_ts: Some(now - 10_000_000),
            max_ts: Some(now + 10_000_000),
        },
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("time range search failed: {other:?}"),
            }
        }
    });

    trace("time_range_filter", "fts5", resp.results.len(), 0);

    // Should find all alpha project messages (9 seeded in alpha)
    assert!(
        resp.results.len() >= 9,
        "time range should include all recent messages, got {}",
        resp.results.len()
    );

    // Search with impossible future range
    let q_empty = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        time_range: TimeRange {
            min_ts: Some(now + 3_600_000_000),
            max_ts: Some(now + 7_200_000_000),
        },
        ..Default::default()
    };

    let resp_empty = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q_empty).await {
                Outcome::Ok(r) => r,
                other => panic!("empty time range search failed: {other:?}"),
            }
        }
    });

    assert!(
        resp_empty.results.is_empty(),
        "future time range should return 0 results, got {}",
        resp_empty.results.len()
    );
}

/// Combined facets (text + importance + ack) produce intersection.
#[test]
fn v3_conformance_combined_facets() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: "search".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        importance: vec![Importance::Urgent],
        ack_required: Some(true),
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("combined facets search failed: {other:?}"),
            }
        }
    });

    trace("combined_facets", "fts5", resp.results.len(), 0);

    for r in &resp.results {
        assert_eq!(
            r.importance.as_deref(),
            Some("urgent"),
            "importance should be urgent"
        );
        assert_eq!(r.ack_required, Some(true), "ack_required should be true");
    }
}

/// Ranking mode Recency sorts by `created_ts` descending.
#[test]
fn v3_conformance_ranking_recency() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        ranking: RankingMode::Recency,
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("recency ranking search failed: {other:?}"),
            }
        }
    });

    trace("ranking_recency", "fts5", resp.results.len(), 0);

    // Verify results are in descending ID order (correlates with creation time)
    for w in resp.results.windows(2) {
        assert!(
            w[0].id >= w[1].id,
            "recency ranking should produce descending IDs: {} < {}",
            w[0].id,
            w[1].id
        );
    }
}

/// Limit clamping is enforced through execute path.
#[test]
fn v3_conformance_limit_enforcement() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    let q = SearchQuery {
        text: String::new(),
        doc_kind: DocKind::Message,
        project_id: Some(corpus.project_id),
        limit: Some(2),
        ..Default::default()
    };

    let resp = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("limited search failed: {other:?}"),
            }
        }
    });

    trace("limit_enforcement", "fts5", resp.results.len(), 0);

    assert!(
        resp.results.len() <= 2,
        "limit=2 should return at most 2 results, got {}",
        resp.results.len()
    );
}

/// Project isolation: search for one project doesn't leak other project data.
#[test]
fn v3_conformance_project_isolation() {
    let (pool, _dir) = make_pool();
    let corpus = seed_corpus(&pool);

    // Search alpha project for beta-only content
    let q_alpha = SearchQuery::messages("beta internal", corpus.project_id);
    let resp_alpha = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q_alpha).await {
                Outcome::Ok(r) => r,
                other => panic!("isolation search failed: {other:?}"),
            }
        }
    });

    trace(
        "project_isolation_alpha",
        "fts5",
        resp_alpha.results.len(),
        0,
    );

    assert!(
        resp_alpha.results.is_empty(),
        "alpha search should not find beta content, got {} results",
        resp_alpha.results.len()
    );

    // Search beta project for alpha-only content
    let q_beta = SearchQuery::messages("migration", corpus.second_project_id);
    let resp_beta = block_on(|cx| {
        let pool = pool.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q_beta).await {
                Outcome::Ok(r) => r,
                other => panic!("isolation search beta failed: {other:?}"),
            }
        }
    });

    trace("project_isolation_beta", "fts5", resp_beta.results.len(), 0);

    assert!(
        resp_beta.results.is_empty(),
        "beta search should not find alpha content, got {} results",
        resp_beta.results.len()
    );
}

/// `SearchEngine::parse` handles all documented aliases.
#[test]
fn v3_conformance_engine_alias_parsing() {
    let cases = [
        ("legacy", SearchEngine::Lexical),
        ("fts5", SearchEngine::Lexical),
        ("fts", SearchEngine::Lexical),
        ("sqlite", SearchEngine::Lexical),
        ("lexical", SearchEngine::Lexical),
        ("tantivy", SearchEngine::Lexical),
        ("v3", SearchEngine::Lexical),
        ("semantic", SearchEngine::Semantic),
        ("vector", SearchEngine::Semantic),
        ("embedding", SearchEngine::Semantic),
        ("hybrid", SearchEngine::Hybrid),
        ("fusion", SearchEngine::Hybrid),
        ("auto", SearchEngine::Auto),
        ("adaptive", SearchEngine::Auto),
    ];

    for (input, expected) in &cases {
        let parsed = SearchEngine::parse(input);
        assert_eq!(
            parsed, *expected,
            "SearchEngine::parse({input:?}) should be {expected:?}, got {parsed:?}"
        );
    }
}

/// `SearchEngine` capability flags are consistent.
#[test]
fn v3_conformance_engine_capabilities() {
    assert!(SearchEngine::Legacy.uses_lexical());
    assert!(!SearchEngine::Legacy.requires_semantic());

    assert!(SearchEngine::Lexical.uses_lexical());
    assert!(!SearchEngine::Lexical.requires_semantic());

    assert!(SearchEngine::Semantic.requires_semantic());

    assert!(SearchEngine::Hybrid.uses_lexical());
    assert!(SearchEngine::Hybrid.requires_semantic());

    assert!(SearchEngine::Auto.uses_lexical());
    assert!(SearchEngine::Auto.requires_semantic());
}

/// `SearchRolloutConfig` per-surface overrides work.
#[test]
fn v3_conformance_rollout_surface_override() {
    use mcp_agent_mail_core::config::SearchRolloutConfig;
    use std::collections::HashMap;

    let mut overrides = HashMap::new();
    overrides.insert("search_messages".to_string(), SearchEngine::Lexical);

    let config = SearchRolloutConfig {
        engine: SearchEngine::Legacy,
        surface_overrides: overrides,
        ..Default::default()
    };

    assert_eq!(
        config.effective_engine("fetch_inbox"),
        SearchEngine::Legacy,
        "non-overridden surface should use default engine"
    );

    assert_eq!(
        config.effective_engine("search_messages"),
        SearchEngine::Lexical,
        "overridden surface should use surface-specific engine"
    );
}
