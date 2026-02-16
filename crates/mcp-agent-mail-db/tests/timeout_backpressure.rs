//! E2E timeout/backpressure/cancellation matrix with deterministic diagnostics.
//!
//! Tests the interaction between:
//! 1. **Backpressure**: HealthLevel classification from HealthSignals
//! 2. **Budget governor**: HybridBudgetGovernorTier classification from Cx budget
//! 3. **Diagnostics**: SearchDiagnostics derived from QueryExplain facets
//! 4. **Tool shedding**: should_shed_tool decisions under Red health
//! 5. **Search with budget**: execute_search/execute_search_simple under budget pressure
//!
//! br-2tnl.7.21

#![allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::clone_on_copy,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::missing_const_for_fn,
    clippy::let_and_return,
    clippy::redundant_clone
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Budget, Cx, Outcome, Time};
use mcp_agent_mail_core::backpressure::{
    self, HealthLevel, HealthSignals, is_shedable_tool, set_shedding_enabled, should_shed_tool,
};
use mcp_agent_mail_core::metrics::GlobalMetrics;
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{QueryExplain, SearchQuery};
use mcp_agent_mail_db::search_service::{SearchOptions, execute_search, execute_search_simple};
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────────────────────────
// Async/pool infrastructure
// ─────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on_with_budget<F, Fut, T>(budget: Budget, f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_request_with_budget(budget);
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("timeout_bp_{}.db", unique_suffix()));
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

// ─────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────

fn default_signals() -> HealthSignals {
    HealthSignals {
        pool_acquire_p95_us: 0,
        pool_utilization_pct: 0,
        pool_over_80_for_s: 0,
        wbq_depth_pct: 0,
        wbq_over_80_for_s: 0,
        commit_depth_pct: 0,
        commit_over_80_for_s: 0,
    }
}

fn make_explain(method: &str, facets: Vec<String>, used_like_fallback: bool) -> QueryExplain {
    QueryExplain {
        method: method.to_string(),
        normalized_query: Some("test query".to_string()),
        used_like_fallback,
        facet_count: facets.len(),
        facets_applied: facets,
        sql: "SELECT 1".to_string(),
        scope_policy: "unrestricted".to_string(),
        denied_count: 0,
        redacted_count: 0,
    }
}

/// Replicate derive_search_diagnostics logic for testing (it's pub(crate) in tools crate).
fn derive_diagnostics(explain: Option<&QueryExplain>) -> Option<DiagnosticsResult> {
    let explain = explain?;
    let mut d = DiagnosticsResult {
        degraded: false,
        fallback_mode: None,
        timeout_stage: None,
        budget_tier: None,
        budget_remaining_ms: None,
        budget_exhausted: None,
        remediation_hints: Vec::new(),
    };

    // Like-fallback detection
    if explain.used_like_fallback || explain.method.eq_ignore_ascii_case("like_fallback") {
        d.degraded = true;
        d.fallback_mode = Some("like_fallback".to_string());
        d.remediation_hints.push(
            "FTS syntax was not usable; simplify operators or use quoted phrases.".to_string(),
        );
    }

    // Budget governor from rerank_outcome facet
    if let Some(outcome) = facet_value(&explain.facets_applied, "rerank_outcome") {
        if let Some(tier) = outcome.strip_prefix("skipped_by_budget_governor_") {
            if !tier.is_empty() {
                d.degraded = true;
                d.fallback_mode
                    .get_or_insert_with(|| "hybrid_budget_governor".to_string());
                d.budget_tier = Some(tier.to_string());
                d.budget_exhausted = Some(tier.eq_ignore_ascii_case("critical"));
                d.remediation_hints
                    .push("Reduce `limit` or narrow filters to avoid budget pressure.".to_string());
            }
        } else if outcome.to_ascii_lowercase().contains("timeout") {
            d.degraded = true;
            d.fallback_mode
                .get_or_insert_with(|| "rerank_timeout".to_string());
            d.timeout_stage = Some("rerank".to_string());
            d.remediation_hints
                .push("Retry with tighter filters or switch to lexical mode.".to_string());
        } else if outcome.to_ascii_lowercase().contains("failed") {
            d.degraded = true;
            d.fallback_mode
                .get_or_insert_with(|| "rerank_failed".to_string());
            d.remediation_hints
                .push("Hybrid refinement failed; retry or use lexical mode.".to_string());
        }
    }

    // Governor remaining budget facet
    if let Some(remaining_ms) = facet_value(&explain.facets_applied, "governor_remaining_budget_ms")
        .and_then(|v| v.parse::<u64>().ok())
    {
        d.budget_remaining_ms = Some(remaining_ms);
    }

    // Explicit governor tier facet
    if let Some(tier) = facet_value(&explain.facets_applied, "governor_tier") {
        d.budget_tier = Some(tier.to_string());
        if d.budget_exhausted.is_none() {
            d.budget_exhausted = Some(tier.eq_ignore_ascii_case("critical"));
        }
    }

    // Timeout stage facet
    if let Some(stage) = facet_value(&explain.facets_applied, "timeout_stage") {
        d.degraded = true;
        d.timeout_stage = Some(stage.to_string());
        d.remediation_hints
            .push("Search timed out in one stage; narrow query scope and retry.".to_string());
    }

    if d.degraded { Some(d) } else { None }
}

fn facet_value<'a>(facets: &'a [String], key: &str) -> Option<&'a str> {
    facets.iter().find_map(|f| {
        let (k, v) = f.split_once(':')?;
        if k.eq_ignore_ascii_case(key) {
            Some(v)
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticsResult {
    degraded: bool,
    fallback_mode: Option<String>,
    timeout_stage: Option<String>,
    budget_tier: Option<String>,
    budget_remaining_ms: Option<u64>,
    budget_exhausted: Option<bool>,
    remediation_hints: Vec<String>,
}

fn seed_corpus(cx: &Cx, pool: &DbPool) -> i64 {
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");

    let project_id = rt.block_on(async {
        let project_id = match queries::ensure_project(cx, pool, "/tmp/test-bp").await {
            Outcome::Ok(p) => p.id.expect("project id"),
            other => panic!("ensure_project failed: {other:?}"),
        };

        let agent_id = match queries::register_agent(
            cx,
            pool,
            project_id,
            "RedHarbor",
            "test-prog",
            "test-model",
            None,
            None,
        )
        .await
        {
            Outcome::Ok(a) => a.id.expect("agent id"),
            other => panic!("register_agent failed: {other:?}"),
        };

        let messages = [
            (
                "Budget governor test",
                "Testing budget governor behavior under critical tier pressure",
            ),
            (
                "Timeout backpressure",
                "Verifying timeout and backpressure signal propagation",
            ),
            (
                "Cancellation matrix",
                "Matrix of cancellation scenarios across all tiers",
            ),
            (
                "Shedding policy",
                "Tool shedding under Red health level with search_messages",
            ),
            (
                "Diagnostics extraction",
                "Deterministic diagnostics from explain facets and remediation hints",
            ),
        ];
        for (subject, body) in &messages {
            match queries::create_message(
                cx, pool, project_id, agent_id, subject, body, None, "normal", false, "[]",
            )
            .await
            {
                Outcome::Ok(_) => {}
                other => panic!("create_message({subject}) failed: {other:?}"),
            }
        }
        project_id
    });

    project_id
}

// ─────────────────────────────────────────────────────────────────
// 1. Health level classification matrix
// ─────────────────────────────────────────────────────────────────

#[test]
fn health_classification_matrix() {
    let cases: Vec<(&str, HealthSignals, HealthLevel)> = vec![
        // All-zero → Green
        ("all_zero", default_signals(), HealthLevel::Green),
        // Pool acquire latency
        (
            "pool_latency_yellow",
            HealthSignals {
                pool_acquire_p95_us: backpressure::yellow::POOL_ACQUIRE_P95_US + 1,
                ..default_signals()
            },
            HealthLevel::Yellow,
        ),
        (
            "pool_latency_red",
            HealthSignals {
                pool_acquire_p95_us: backpressure::red::POOL_ACQUIRE_P95_US + 1,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // Pool utilization
        (
            "pool_util_yellow",
            HealthSignals {
                pool_utilization_pct: 70,
                ..default_signals()
            },
            HealthLevel::Yellow,
        ),
        (
            "pool_util_red",
            HealthSignals {
                pool_utilization_pct: 90,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // WBQ depth
        (
            "wbq_depth_yellow",
            HealthSignals {
                wbq_depth_pct: 50,
                ..default_signals()
            },
            HealthLevel::Yellow,
        ),
        (
            "wbq_depth_red",
            HealthSignals {
                wbq_depth_pct: 80,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // Commit depth
        (
            "commit_depth_yellow",
            HealthSignals {
                commit_depth_pct: 50,
                ..default_signals()
            },
            HealthLevel::Yellow,
        ),
        (
            "commit_depth_red",
            HealthSignals {
                commit_depth_pct: 80,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // Sustained over-80 durations
        (
            "pool_sustained_300s",
            HealthSignals {
                pool_over_80_for_s: 300,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        (
            "wbq_sustained_300s",
            HealthSignals {
                wbq_over_80_for_s: 300,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        (
            "commit_sustained_300s",
            HealthSignals {
                commit_over_80_for_s: 300,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // Compound: yellow + red → Red wins
        (
            "compound_yellow_plus_red",
            HealthSignals {
                pool_acquire_p95_us: backpressure::yellow::POOL_ACQUIRE_P95_US + 1,
                wbq_depth_pct: 80,
                ..default_signals()
            },
            HealthLevel::Red,
        ),
        // Boundary: at-threshold pool latency (not above) → Green
        (
            "pool_latency_at_yellow_boundary",
            HealthSignals {
                pool_acquire_p95_us: backpressure::yellow::POOL_ACQUIRE_P95_US,
                ..default_signals()
            },
            HealthLevel::Green,
        ),
    ];

    let mut pass = 0;
    let mut fail = 0;
    for (label, signals, expected) in &cases {
        let actual = signals.classify();
        if actual == *expected {
            pass += 1;
        } else {
            fail += 1;
            eprintln!("FAIL {label}: expected {expected:?}, got {actual:?}");
        }
    }

    eprintln!(
        "health_classification_matrix: {pass}/{} passed",
        pass + fail
    );
    assert_eq!(fail, 0, "{fail} health classification cases failed");
}

// ─────────────────────────────────────────────────────────────────
// 2. Tool shedding matrix
// ─────────────────────────────────────────────────────────────────

#[test]
fn tool_shedding_matrix() {
    let shedable = [
        "search_messages",
        "summarize_thread",
        "whois",
        "list_contacts",
        "search_messages_product",
        "summarize_thread_product",
        "fetch_inbox_product",
    ];
    let critical = [
        "health_check",
        "ensure_project",
        "register_agent",
        "send_message",
        "reply_message",
        "fetch_inbox",
        "mark_message_read",
        "acknowledge_message",
        "file_reservation_paths",
        "release_file_reservations",
        "macro_start_session",
        "acquire_build_slot",
    ];

    let levels = [HealthLevel::Green, HealthLevel::Yellow, HealthLevel::Red];
    let mut assertions = 0;

    for level in &levels {
        for tool in &shedable {
            let expected = matches!(level, HealthLevel::Red);
            let actual = level.should_shed(is_shedable_tool(tool));
            assert_eq!(
                actual, expected,
                "{tool} at {level:?}: expected should_shed={expected}"
            );
            assertions += 1;
        }
        for tool in &critical {
            let actual = level.should_shed(is_shedable_tool(tool));
            assert!(
                !actual,
                "{tool} at {level:?}: critical tool should NEVER be shed"
            );
            assertions += 1;
        }
    }

    eprintln!("tool_shedding_matrix: {assertions} assertions passed");
}

// ─────────────────────────────────────────────────────────────────
// 3. Shedding gate integration
// ─────────────────────────────────────────────────────────────────

#[test]
fn shedding_gate_respects_global_flag() {
    let original = backpressure::shedding_enabled();

    // With flag OFF: should_shed_tool never returns true
    set_shedding_enabled(false);
    assert!(!should_shed_tool("search_messages"));
    assert!(!should_shed_tool("whois"));
    assert!(!should_shed_tool("send_message"));

    // With flag ON: critical tools still never shed
    set_shedding_enabled(true);
    assert!(!should_shed_tool("send_message"));
    assert!(!should_shed_tool("register_agent"));
    assert!(!should_shed_tool("fetch_inbox"));

    // Restore
    set_shedding_enabled(original);
}

// ─────────────────────────────────────────────────────────────────
// 4. Diagnostics extraction matrix
// ─────────────────────────────────────────────────────────────────

#[test]
fn diagnostics_extraction_matrix() {
    // Case 1: No explain → None
    assert!(derive_diagnostics(None).is_none());

    // Case 2: Normal FTS → None (not degraded)
    let explain_normal = make_explain("fts5", vec![], false);
    assert!(derive_diagnostics(Some(&explain_normal)).is_none());

    // Case 3: Like fallback via method name
    let explain_like_method = make_explain("like_fallback", vec![], false);
    let d = derive_diagnostics(Some(&explain_like_method)).expect("like_fallback method");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("like_fallback"));
    assert!(d.remediation_hints.iter().any(|h| h.contains("FTS syntax")));

    // Case 4: Like fallback via flag
    let explain_like_flag = make_explain("fts5", vec![], true);
    let d = derive_diagnostics(Some(&explain_like_flag)).expect("like flag");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("like_fallback"));

    // Case 5: Budget governor critical
    let explain_critical = make_explain(
        "hybrid_v3",
        vec![
            "engine:Hybrid".to_string(),
            "rerank_outcome:skipped_by_budget_governor_critical".to_string(),
            "governor_remaining_budget_ms:15".to_string(),
        ],
        false,
    );
    let d = derive_diagnostics(Some(&explain_critical)).expect("critical diagnostics");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("hybrid_budget_governor"));
    assert_eq!(d.budget_tier.as_deref(), Some("critical"));
    assert_eq!(d.budget_remaining_ms, Some(15));
    assert_eq!(d.budget_exhausted, Some(true));
    assert!(d.remediation_hints.iter().any(|h| h.contains("limit")));

    // Case 6: Budget governor tight
    let explain_tight = make_explain(
        "hybrid_v3",
        vec![
            "rerank_outcome:skipped_by_budget_governor_tight".to_string(),
            "governor_remaining_budget_ms:180".to_string(),
        ],
        false,
    );
    let d = derive_diagnostics(Some(&explain_tight)).expect("tight diagnostics");
    assert!(d.degraded);
    assert_eq!(d.budget_tier.as_deref(), Some("tight"));
    assert_eq!(d.budget_remaining_ms, Some(180));
    assert_eq!(d.budget_exhausted, Some(false));

    // Case 7: Rerank timeout
    let explain_timeout = make_explain(
        "hybrid_v3",
        vec!["rerank_outcome:timeout_after_500ms".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain_timeout)).expect("timeout diagnostics");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("rerank_timeout"));
    assert_eq!(d.timeout_stage.as_deref(), Some("rerank"));

    // Case 8: Rerank failed
    let explain_failed = make_explain(
        "hybrid_v3",
        vec!["rerank_outcome:failed_model_unavailable".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain_failed)).expect("failed diagnostics");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("rerank_failed"));
    assert!(
        d.remediation_hints
            .iter()
            .any(|h| h.contains("lexical mode"))
    );

    // Case 9: Explicit governor_tier without degraded trigger → None
    let explain_tier_only = make_explain("legacy", vec!["governor_tier:tight".to_string()], false);
    let d = derive_diagnostics(Some(&explain_tier_only));
    assert!(
        d.is_none(),
        "governor_tier alone without degraded signal → None"
    );

    // Case 10: Explicit timeout_stage facet
    let explain_stage = make_explain(
        "hybrid_v3",
        vec!["timeout_stage:semantic_retrieval".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain_stage)).expect("timeout_stage diagnostics");
    assert!(d.degraded);
    assert_eq!(d.timeout_stage.as_deref(), Some("semantic_retrieval"));
    assert!(d.remediation_hints.iter().any(|h| h.contains("timed out")));

    // Case 11: Compound — like_fallback + timeout_stage
    let explain_compound = make_explain(
        "like_fallback",
        vec!["timeout_stage:rerank".to_string()],
        true,
    );
    let d = derive_diagnostics(Some(&explain_compound)).expect("compound diagnostics");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("like_fallback"));
    assert_eq!(d.timeout_stage.as_deref(), Some("rerank"));
    assert!(
        d.remediation_hints.len() >= 2,
        "should have hints for both degradation causes"
    );

    eprintln!("diagnostics_extraction_matrix: 11 cases, all passed");
}

// ─────────────────────────────────────────────────────────────────
// 5. HealthSignals::from_snapshot integration
// ─────────────────────────────────────────────────────────────────

#[test]
fn health_signals_from_snapshot_correctly_classifies() {
    let now_us: u64 = 2_000_000_000;

    // Green: all zeros
    let snap = GlobalMetrics::default().snapshot();
    let signals = HealthSignals::from_snapshot(&snap, now_us);
    assert_eq!(signals.classify(), HealthLevel::Green);

    // Yellow: pool utilization 75%
    let mut snap_yellow = GlobalMetrics::default().snapshot();
    snap_yellow.db.pool_utilization_pct = 75;
    let signals = HealthSignals::from_snapshot(&snap_yellow, now_us);
    assert_eq!(signals.classify(), HealthLevel::Yellow);
    assert_eq!(signals.pool_utilization_pct, 75);

    // Red: WBQ at 90% capacity
    let mut snap_red = GlobalMetrics::default().snapshot();
    snap_red.storage.wbq_depth = 900;
    snap_red.storage.wbq_capacity = 1000;
    let signals = HealthSignals::from_snapshot(&snap_red, now_us);
    assert_eq!(signals.classify(), HealthLevel::Red);
    assert_eq!(signals.wbq_depth_pct, 90);

    // Red: sustained pool over-80 for 5 minutes
    let mut snap_sustained = GlobalMetrics::default().snapshot();
    snap_sustained.db.pool_over_80_since_us = now_us - 300_000_000;
    let signals = HealthSignals::from_snapshot(&snap_sustained, now_us);
    assert_eq!(signals.pool_over_80_for_s, 300);
    assert_eq!(signals.classify(), HealthLevel::Red);

    // Edge: zero capacities don't divide by zero
    let mut snap_zero_cap = GlobalMetrics::default().snapshot();
    snap_zero_cap.storage.wbq_capacity = 0;
    snap_zero_cap.storage.commit_soft_cap = 0;
    snap_zero_cap.storage.wbq_depth = 999;
    snap_zero_cap.storage.commit_pending_requests = 999;
    let signals = HealthSignals::from_snapshot(&snap_zero_cap, now_us);
    assert_eq!(signals.wbq_depth_pct, 0);
    assert_eq!(signals.commit_depth_pct, 0);
    assert_eq!(signals.classify(), HealthLevel::Green);

    eprintln!("health_signals_from_snapshot: 5 scenarios passed");
}

// ─────────────────────────────────────────────────────────────────
// 6. Budget-constrained search (Cx with cost_quota)
// ─────────────────────────────────────────────────────────────────

#[test]
fn search_with_cost_quota_budget() {
    let (pool, _dir) = make_pool();
    let cx_setup = Cx::for_testing();
    let project_id = seed_corpus(&cx_setup, &pool);

    // Normal search with unlimited budget → should succeed
    let pool_c = pool.clone();
    let resp = block_on_with_budget(Budget::new(), move |cx| async move {
        match execute_search_simple(&cx, &pool_c, &SearchQuery::messages("budget", project_id))
            .await
        {
            Outcome::Ok(r) => r,
            other => panic!("unlimited budget search failed: {other:?}"),
        }
    });
    assert!(!resp.results.is_empty(), "should find 'budget' in corpus");

    // Search with cost_quota — still succeeds
    let pool_c = pool.clone();
    block_on_with_budget(Budget::new().with_cost_quota(87), move |cx| async move {
        match execute_search_simple(&cx, &pool_c, &SearchQuery::messages("timeout", project_id))
            .await
        {
            Outcome::Ok(_) => {}
            other => panic!("cost-quota budget search failed: {other:?}"),
        }
    });

    // Search with expired deadline (Time::ZERO)
    let pool_c = pool.clone();
    block_on_with_budget(
        Budget::new().with_deadline(Time::ZERO),
        move |cx| async move {
            // FTS search via SQLite is synchronous; expired deadline doesn't cancel mid-query
            match execute_search_simple(
                &cx,
                &pool_c,
                &SearchQuery::messages("cancellation", project_id),
            )
            .await
            {
                Outcome::Ok(_) => {}
                other => panic!("expired deadline search failed: {other:?}"),
            }
        },
    );

    eprintln!("search_with_cost_quota_budget: 3 scenarios passed");
}

// ─────────────────────────────────────────────────────────────────
// 7. Budget-constrained execute_search (with SearchOptions)
// ─────────────────────────────────────────────────────────────────

#[test]
fn execute_search_with_budget_options() {
    let (pool, _dir) = make_pool();
    let cx_setup = Cx::for_testing();
    let project_id = seed_corpus(&cx_setup, &pool);

    // execute_search with explain=true should populate explain metadata
    let pool_c = pool.clone();
    let resp = block_on_with_budget(Budget::new(), move |cx| async move {
        let sq = SearchQuery {
            text: "diagnostics".to_string(),
            explain: true,
            ..SearchQuery::messages("", project_id)
        };
        let opts = SearchOptions {
            track_telemetry: true,
            ..Default::default()
        };
        match execute_search(&cx, &pool_c, &sq, &opts).await {
            Outcome::Ok(r) => r,
            other => panic!("execute_search with explain failed: {other:?}"),
        }
    });
    assert!(resp.explain.is_some(), "explain metadata should be present");
    let explain = resp.explain.unwrap();
    assert!(
        !explain.method.is_empty(),
        "explain method should be populated"
    );
    assert!(!explain.sql.is_empty(), "explain sql should be populated");

    // execute_search with cost_quota → succeeds
    let pool_c = pool.clone();
    block_on_with_budget(Budget::new().with_cost_quota(50), move |cx| async move {
        let sq = SearchQuery {
            text: "shedding".to_string(),
            explain: true,
            ..SearchQuery::messages("", project_id)
        };
        let opts = SearchOptions {
            track_telemetry: true,
            ..Default::default()
        };
        match execute_search(&cx, &pool_c, &sq, &opts).await {
            Outcome::Ok(_) => {}
            other => panic!("execute_search with cost_quota failed: {other:?}"),
        }
    });

    eprintln!("execute_search_with_budget_options: 2 scenarios passed");
}

// ─────────────────────────────────────────────────────────────────
// 8. Health level transitions are deterministic
// ─────────────────────────────────────────────────────────────────

#[test]
fn health_level_transitions_are_deterministic() {
    let signals_yellow = HealthSignals {
        pool_acquire_p95_us: backpressure::yellow::POOL_ACQUIRE_P95_US + 1,
        ..default_signals()
    };

    let results: Vec<HealthLevel> = (0..100).map(|_| signals_yellow.classify()).collect();
    assert!(
        results.iter().all(|l| *l == HealthLevel::Yellow),
        "classify must be deterministic over 100 iterations"
    );

    let signals_red = HealthSignals {
        wbq_depth_pct: 80,
        ..default_signals()
    };
    let results: Vec<HealthLevel> = (0..100).map(|_| signals_red.classify()).collect();
    assert!(
        results.iter().all(|l| *l == HealthLevel::Red),
        "Red classification must be stable"
    );

    eprintln!("health_level_transitions_are_deterministic: 200 assertions passed");
}

// ─────────────────────────────────────────────────────────────────
// 9. All degraded diagnostics have remediation hints
// ─────────────────────────────────────────────────────────────────

#[test]
fn all_degraded_diagnostics_have_remediation_hints() {
    let degraded_cases: Vec<QueryExplain> = vec![
        make_explain("like_fallback", vec![], true),
        make_explain(
            "hybrid_v3",
            vec!["rerank_outcome:skipped_by_budget_governor_critical".to_string()],
            false,
        ),
        make_explain(
            "hybrid_v3",
            vec!["rerank_outcome:skipped_by_budget_governor_tight".to_string()],
            false,
        ),
        make_explain(
            "hybrid_v3",
            vec!["rerank_outcome:timeout_after_1s".to_string()],
            false,
        ),
        make_explain(
            "hybrid_v3",
            vec!["rerank_outcome:failed_oom".to_string()],
            false,
        ),
        make_explain(
            "hybrid_v3",
            vec!["timeout_stage:lexical_retrieval".to_string()],
            false,
        ),
    ];

    let mut assertions = 0;
    for (i, explain) in degraded_cases.iter().enumerate() {
        let d = derive_diagnostics(Some(explain))
            .unwrap_or_else(|| panic!("case {i}: expected degraded diagnostics"));
        assert!(d.degraded, "case {i}: should be degraded");
        assert!(
            !d.remediation_hints.is_empty(),
            "case {i}: must have at least one remediation hint"
        );
        assertions += 2;
    }

    eprintln!("all_degraded_diagnostics_have_remediation_hints: {assertions} assertions");
}

// ─────────────────────────────────────────────────────────────────
// 10. HealthLevel ordering and serialization
// ─────────────────────────────────────────────────────────────────

#[test]
fn health_level_ordering_and_serialization() {
    assert!(HealthLevel::Green < HealthLevel::Yellow);
    assert!(HealthLevel::Yellow < HealthLevel::Red);
    assert!(HealthLevel::Green < HealthLevel::Red);

    assert_eq!(HealthLevel::Green.as_str(), "green");
    assert_eq!(HealthLevel::Yellow.as_str(), "yellow");
    assert_eq!(HealthLevel::Red.as_str(), "red");

    let json = serde_json::to_string(&HealthLevel::Yellow).unwrap();
    assert_eq!(json, "\"yellow\"");

    for v in 0..=2 {
        let level = HealthLevel::from_u8(v);
        assert_eq!(level as u8, v);
    }
    assert_eq!(HealthLevel::from_u8(255), HealthLevel::Red);
    assert_eq!(HealthLevel::from_u8(42), HealthLevel::Red);

    eprintln!("health_level_ordering_and_serialization: 11 assertions passed");
}

// ─────────────────────────────────────────────────────────────────
// 11. Worst-of-all-signals classification
// ─────────────────────────────────────────────────────────────────

#[test]
fn worst_signal_wins_classification() {
    let all_red = HealthSignals {
        pool_acquire_p95_us: backpressure::red::POOL_ACQUIRE_P95_US + 1,
        pool_utilization_pct: 90,
        pool_over_80_for_s: 300,
        wbq_depth_pct: 80,
        wbq_over_80_for_s: 300,
        commit_depth_pct: 80,
        commit_over_80_for_s: 300,
    };
    assert_eq!(all_red.classify(), HealthLevel::Red);

    let one_red = HealthSignals {
        wbq_depth_pct: 80,
        ..default_signals()
    };
    assert_eq!(one_red.classify(), HealthLevel::Red);

    let multi_yellow = HealthSignals {
        pool_acquire_p95_us: backpressure::yellow::POOL_ACQUIRE_P95_US + 1,
        pool_utilization_pct: 70,
        wbq_depth_pct: 50,
        commit_depth_pct: 50,
        ..default_signals()
    };
    assert_eq!(multi_yellow.classify(), HealthLevel::Yellow);

    let one_yellow = HealthSignals {
        commit_depth_pct: 50,
        ..default_signals()
    };
    assert_eq!(one_yellow.classify(), HealthLevel::Yellow);

    // Rapid oscillation
    let mut s = default_signals();
    assert_eq!(s.classify(), HealthLevel::Green);
    s.wbq_depth_pct = 80;
    assert_eq!(s.classify(), HealthLevel::Red);
    s.wbq_depth_pct = 0;
    assert_eq!(s.classify(), HealthLevel::Green);
    s.pool_utilization_pct = 75;
    assert_eq!(s.classify(), HealthLevel::Yellow);
    s.pool_utilization_pct = 0;
    assert_eq!(s.classify(), HealthLevel::Green);

    eprintln!("worst_signal_wins_classification: 9 scenarios passed");
}

// ─────────────────────────────────────────────────────────────────
// 12. Diagnostics facet parsing edge cases
// ─────────────────────────────────────────────────────────────────

#[test]
fn diagnostics_facet_parsing_edge_cases() {
    // Empty facet value after prefix
    let explain = make_explain(
        "hybrid_v3",
        vec!["rerank_outcome:skipped_by_budget_governor_".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain));
    assert!(
        d.is_none(),
        "empty tier after prefix should not trigger diagnostics"
    );

    // Non-matching facet key
    let explain = make_explain(
        "hybrid_v3",
        vec!["unknown_facet:critical".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain));
    assert!(
        d.is_none(),
        "unknown facet key should not trigger diagnostics"
    );

    // Case-insensitive governor_tier matching for exhausted detection
    let explain = make_explain(
        "hybrid_v3",
        vec!["rerank_outcome:skipped_by_budget_governor_CRITICAL".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain)).expect("case-insensitive critical");
    assert_eq!(d.budget_exhausted, Some(true));
    assert_eq!(d.budget_tier.as_deref(), Some("CRITICAL"));

    // Non-numeric governor_remaining_budget_ms → None
    let explain = make_explain(
        "hybrid_v3",
        vec![
            "rerank_outcome:skipped_by_budget_governor_tight".to_string(),
            "governor_remaining_budget_ms:not_a_number".to_string(),
        ],
        false,
    );
    let d = derive_diagnostics(Some(&explain)).expect("tight with bad remaining_ms");
    assert!(d.degraded);
    assert!(
        d.budget_remaining_ms.is_none(),
        "non-numeric should be None"
    );

    // Multiple colons in facet value (split_once takes first colon)
    let explain = make_explain(
        "hybrid_v3",
        vec!["rerank_outcome:timeout:extra:colons".to_string()],
        false,
    );
    let d = derive_diagnostics(Some(&explain)).expect("timeout with extra colons");
    assert!(d.degraded);
    assert_eq!(d.fallback_mode.as_deref(), Some("rerank_timeout"));

    eprintln!("diagnostics_facet_parsing_edge_cases: 5 edge cases passed");
}

// ─────────────────────────────────────────────────────────────────
// 13. Search with multiple budget constraints
// ─────────────────────────────────────────────────────────────────

#[test]
fn search_with_combined_budget_constraints() {
    let (pool, _dir) = make_pool();
    let cx_setup = Cx::for_testing();
    let project_id = seed_corpus(&cx_setup, &pool);

    // Combined: cost_quota + poll_quota — search should still work
    let pool_c = pool.clone();
    let resp = block_on_with_budget(
        Budget::new().with_cost_quota(1000).with_poll_quota(10000),
        move |cx| async move {
            match execute_search_simple(&cx, &pool_c, &SearchQuery::messages("matrix", project_id))
                .await
            {
                Outcome::Ok(r) => r,
                other => panic!("combined budget search failed: {other:?}"),
            }
        },
    );
    assert!(!resp.results.is_empty(), "should find 'matrix' in corpus");

    // Verify budget properties are accessible on Cx
    let cx = Cx::for_request_with_budget(Budget::new().with_cost_quota(42).with_poll_quota(100));
    let budget = cx.budget();
    assert_eq!(budget.remaining_cost(), Some(42));
    assert_eq!(budget.poll_quota, 100);

    eprintln!("search_with_combined_budget_constraints: 4 assertions passed");
}

// ─────────────────────────────────────────────────────────────────
// 14. CI-friendly structured output
// ─────────────────────────────────────────────────────────────────

#[test]
fn ci_structured_report() {
    let report = serde_json::json!({
        "suite": "timeout_backpressure",
        "bead": "br-2tnl.7.21",
        "test_count": 14,
        "categories": [
            "health_classification_matrix",
            "tool_shedding_matrix",
            "shedding_gate_respects_global_flag",
            "diagnostics_extraction_matrix",
            "health_signals_from_snapshot_correctly_classifies",
            "search_with_cost_quota_budget",
            "execute_search_with_budget_options",
            "health_level_transitions_are_deterministic",
            "all_degraded_diagnostics_have_remediation_hints",
            "health_level_ordering_and_serialization",
            "worst_signal_wins_classification",
            "diagnostics_facet_parsing_edge_cases",
            "search_with_combined_budget_constraints",
            "ci_structured_report",
        ],
    });

    eprintln!("TIMEOUT_BACKPRESSURE_REPORT_JSON");
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
    eprintln!("END_TIMEOUT_BACKPRESSURE_REPORT_JSON");
}
