//! Search data-plane conformance + fuzz-style test suite (br-3vwi.10.6).
//!
//! Three test families:
//!
//! 1. **Conformance**: deterministic assertions on planner behavior, SQL generation,
//!    facet handling, and cursor encoding — the "contract" of the search data-plane.
//! 2. **Fuzz / hostile input**: malformed queries, SQL injection attempts, Unicode
//!    edge cases, and pathologically long inputs — everything the planner must handle
//!    without panicking or returning SQL errors.
//! 3. **Parity**: round-trip tests verifying that `execute_search_simple` produces
//!    results consistent with the plan produced by `plan_search`.
//!
//! Every failing assertion emits a structured trace line:
//! ```text
//! TRACE query=<raw> | norm=<normalized> | method=<plan_method> | facets=<list> | rows=<n> | elapsed_us=<n>
//! ```

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::redundant_clone
)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::queries::{extract_like_terms, sanitize_fts_query};
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, PlanMethod, RankingMode, SearchCursor, SearchQuery, SearchResponse,
    TimeRange, plan_search,
};
use mcp_agent_mail_db::search_service::execute_search_simple;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, now_micros, queries};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("search_conf_{}.db", unique_suffix()));
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

/// Structured trace output for debugging failures.
fn trace_line(
    raw: &str,
    plan_method: &str,
    normalized: Option<&str>,
    facets: &[String],
    rows: usize,
) {
    eprintln!(
        "TRACE query={:?} | norm={:?} | method={} | facets={:?} | rows={}",
        raw,
        normalized.unwrap_or("<none>"),
        plan_method,
        facets,
        rows,
    );
}

/// Seed a project with messages for round-trip parity tests.
fn seed_parity_corpus(pool: &DbPool) -> (i64, i64, i64) {
    let (project_id, sender_id, recipient_id) = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let project = match queries::ensure_project(&cx, &pool, "/test/parity-conf").await {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = project.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "RedPeak",
                "claude-code",
                "opus-4.6",
                Some("conformance sender"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register sender failed: {other:?}"),
            };
            let recipient = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "BlueLake",
                "claude-code",
                "opus-4.6",
                Some("conformance recipient"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register recipient failed: {other:?}"),
            };

            (pid, sender.id.unwrap(), recipient.id.unwrap())
        }
    });

    // Seed 20 messages with varied content
    let messages: Vec<(&str, &str, &str, bool)> = vec![
        (
            "Database migration plan",
            "Steps for migrating the schema from v2 to v3",
            "high",
            true,
        ),
        (
            "Weekly sync notes",
            "Discussed performance regression in FTS5 queries",
            "normal",
            false,
        ),
        (
            "Bug report: null pointer",
            "NullPointerException in search indexer when query is empty",
            "urgent",
            true,
        ),
        (
            "Feature request: dark mode",
            "Users want dark mode support in the dashboard",
            "low",
            false,
        ),
        (
            "Security audit findings",
            "Found SQL injection vector in legacy search endpoint",
            "urgent",
            true,
        ),
        (
            "Performance benchmarks",
            "P95 latency improved from 12ms to 3ms after WAL tuning",
            "high",
            false,
        ),
        (
            "Release notes v4.2",
            "Includes FTS5 upgrade and cursor pagination fixes",
            "normal",
            false,
        ),
        (
            "Onboarding guide draft",
            "New agent onboarding process with step-by-step walkthrough",
            "low",
            false,
        ),
        (
            "Incident postmortem",
            "Root cause: unbounded query expansion caused OOM",
            "high",
            true,
        ),
        (
            "API compatibility check",
            "Verified backward compat with Python client v3.x",
            "normal",
            false,
        ),
        (
            "Thread digest implementation",
            "Added thread digest with summarization for long threads",
            "normal",
            false,
        ),
        (
            "Reservation conflict alert",
            "Multiple agents editing the same file simultaneously",
            "high",
            true,
        ),
        (
            "Git archive maintenance",
            "Pruned stale branches and optimized pack files",
            "low",
            false,
        ),
        (
            "Search ranking tuning",
            "Adjusted BM25 weights for subject vs body relevance",
            "normal",
            false,
        ),
        (
            "CI pipeline update",
            "Added clippy + fmt checks to pre-merge gate",
            "normal",
            false,
        ),
        (
            "Unicode handling fix",
            "Fixed UTF-8 encoding in FTS tokenizer for CJK characters",
            "high",
            false,
        ),
        (
            "Load test results",
            "System handled 10K concurrent searches without degradation",
            "normal",
            false,
        ),
        (
            "Contact policy enforcement",
            "Block-all agents now correctly hidden from search",
            "high",
            true,
        ),
        (
            "Anomaly detection MVP",
            "Initial heuristics for latency spikes and error rate anomalies",
            "normal",
            false,
        ),
        (
            "Export pipeline redesign",
            "Switched to streaming export for large mailboxes",
            "normal",
            false,
        ),
    ];

    for (i, (subject, body, importance, ack)) in messages.iter().enumerate() {
        let pool_c = pool.clone();
        let thread_id = if i % 3 == 0 {
            Some(format!("thread-{}", i / 3))
        } else {
            None
        };
        block_on(|cx| {
            let pool = pool_c;
            async move {
                match queries::create_message(
                    &cx,
                    &pool,
                    project_id,
                    sender_id,
                    subject,
                    body,
                    thread_id.as_deref(),
                    importance,
                    *ack,
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

    (project_id, sender_id, recipient_id)
}

// ════════════════════════════════════════════════════════════════════
// 1. CONFORMANCE: planner contract tests
// ════════════════════════════════════════════════════════════════════

/// Verify planner method selection is deterministic for each query shape.
#[test]
fn conformance_method_selection() {
    let cases: Vec<(&str, Option<i64>, PlanMethod)> = vec![
        // FTS: non-empty text that sanitizes to something
        ("migration", Some(1), PlanMethod::Fts),
        ("database schema", Some(1), PlanMethod::Fts),
        ("FTS5 OR sqlite", Some(1), PlanMethod::Fts),
        // Empty: no text, no facets
        ("", None, PlanMethod::Empty),
        // FilterOnly: no text but has facets
        ("", Some(1), PlanMethod::FilterOnly),
        // Empty text with importance still = FilterOnly
        ("", Some(1), PlanMethod::FilterOnly),
    ];

    for (text, project_id, expected_method) in &cases {
        let q = SearchQuery {
            text: text.to_string(),
            doc_kind: DocKind::Message,
            project_id: *project_id,
            ..Default::default()
        };
        let plan = plan_search(&q);
        assert_eq!(
            plan.method, *expected_method,
            "text={text:?} project_id={project_id:?}: expected {expected_method:?}, got {:?}",
            plan.method
        );
    }
}

/// Verify facet tracking is complete and accurate.
#[test]
fn conformance_facet_tracking() {
    // Query with all facets set
    let q = SearchQuery {
        text: "test".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        importance: vec![Importance::High, Importance::Urgent],
        ack_required: Some(true),
        thread_id: Some("thread-1".to_string()),
        agent_name: Some("RedPeak".to_string()),
        time_range: TimeRange {
            min_ts: Some(0),
            max_ts: Some(now_micros()),
        },
        ..Default::default()
    };
    let plan = plan_search(&q);

    // All facets should be tracked
    assert!(
        plan.facets_applied.contains(&"project_id".to_string()),
        "missing project_id facet: {:?}",
        plan.facets_applied
    );
    assert!(
        plan.facets_applied.contains(&"importance".to_string()),
        "missing importance facet: {:?}",
        plan.facets_applied
    );
    assert!(
        plan.facets_applied.contains(&"ack_required".to_string()),
        "missing ack_required facet: {:?}",
        plan.facets_applied
    );
    assert!(
        plan.facets_applied.contains(&"thread_id".to_string()),
        "missing thread_id facet: {:?}",
        plan.facets_applied
    );
    assert!(
        plan.facets_applied.contains(&"time_range_min".to_string()),
        "missing time_range_min facet: {:?}",
        plan.facets_applied
    );
    assert!(
        plan.facets_applied.contains(&"time_range_max".to_string()),
        "missing time_range_max facet: {:?}",
        plan.facets_applied
    );
}

/// Verify explain output is populated when requested.
#[test]
fn conformance_explain_contract() {
    let q = SearchQuery {
        text: "migration".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        explain: true,
        ..Default::default()
    };
    let plan = plan_search(&q);
    let explain = plan.explain();

    assert_eq!(explain.method, "fts5");
    assert!(explain.normalized_query.is_some());
    assert!(!explain.sql.is_empty());
    assert!(explain.facets_applied.contains(&"project_id".to_string()));
    assert!(explain.facet_count >= 1);
    assert!(!explain.used_like_fallback);
}

/// Verify cursor encode/decode is lossless.
#[test]
fn conformance_cursor_roundtrip() {
    let test_cases = [
        (0.0_f64, 0_i64),
        (-1.5, 42),
        (f64::MIN, i64::MAX),
        (f64::MAX, i64::MIN),
        (-0.0, 1),
        (f64::INFINITY, 100),
        (f64::NEG_INFINITY, -100),
    ];
    for (score, id) in &test_cases {
        let cursor = SearchCursor {
            score: *score,
            id: *id,
        };
        let encoded = cursor.encode();
        let decoded = SearchCursor::decode(&encoded).expect("decode should succeed");
        assert_eq!(decoded.id, *id, "id mismatch for score={score}");
        // NaN check: if score is NaN, decoded score should also be NaN
        if score.is_nan() {
            assert!(decoded.score.is_nan());
        } else {
            assert_eq!(
                decoded.score.to_bits(),
                score.to_bits(),
                "score bits mismatch"
            );
        }
    }
}

/// Verify cursor decode rejects malformed input.
#[test]
fn conformance_cursor_decode_rejects_garbage() {
    let bad_cursors = [
        "",
        "garbage",
        "s:i",
        "snotahex:i42",
        "s0000000000000000:inotanumber",
        "s0000000000000000",
        ":i42",
        "s0000000000000000:i",
        "s0000000000000000:i42:extra",
    ];
    for bad in &bad_cursors {
        assert!(
            SearchCursor::decode(bad).is_none(),
            "should reject malformed cursor: {bad:?}"
        );
    }
}

/// Verify limit clamping.
#[test]
fn conformance_limit_clamping() {
    let cases = [
        (None, 50_usize),
        (Some(0), 1),
        (Some(1), 1),
        (Some(50), 50),
        (Some(1000), 1000),
        (Some(2000), 1000),
        (Some(usize::MAX), 1000),
    ];
    for (limit, expected) in &cases {
        let q = SearchQuery {
            limit: *limit,
            ..Default::default()
        };
        assert_eq!(
            q.effective_limit(),
            *expected,
            "limit={limit:?} should clamp to {expected}"
        );
    }
}

/// Verify all three doc kinds produce valid plans.
#[test]
fn conformance_doc_kinds() {
    for kind in [DocKind::Message, DocKind::Agent, DocKind::Project] {
        let q = SearchQuery {
            text: "test".to_string(),
            doc_kind: kind,
            project_id: Some(1),
            ..Default::default()
        };
        let plan = plan_search(&q);
        assert_eq!(
            plan.method,
            PlanMethod::Fts,
            "{kind:?} with text should use FTS"
        );
        assert!(!plan.sql.is_empty(), "{kind:?} plan should have SQL");
        assert!(
            !plan.params.is_empty(),
            "{kind:?} plan should have at least 1 param"
        );
    }
}

/// Verify ranking mode affects SQL ordering.
#[test]
fn conformance_ranking_modes() {
    // Relevance mode: should order by BM25 score
    let q_rel = SearchQuery::messages("test", 1);
    let plan_rel = plan_search(&q_rel);
    assert!(
        plan_rel.sql.contains("score") || plan_rel.sql.contains("bm25"),
        "relevance plan should reference score/bm25"
    );

    // Recency mode with facets-only: should order by created_ts
    let q_rec = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(1),
        importance: vec![Importance::Normal],
        ranking: RankingMode::Recency,
        ..Default::default()
    };
    let plan_recency = plan_search(&q_rec);
    assert!(
        plan_recency.sql.contains("created_ts DESC"),
        "recency/filter plan should sort by created_ts DESC"
    );
}

// ════════════════════════════════════════════════════════════════════
// 2. FUZZ: hostile / malformed input safety
// ════════════════════════════════════════════════════════════════════

/// FTS sanitizer must never panic, even on pathological inputs.
#[test]
fn fuzz_sanitizer_no_panic() {
    let hostile_inputs = [
        // Empty / whitespace
        "",
        " ",
        "   ",
        "\t\n\r",
        // SQL injection attempts
        "'; DROP TABLE messages; --",
        "\" OR 1=1 --",
        "UNION SELECT * FROM sqlite_master",
        "1; ATTACH DATABASE '/etc/passwd' AS pwn",
        // FTS5 syntax attacks
        "NEAR(a b, 999999999)",
        "NEAR/999999999",
        "{col1 col2}: test",
        "col:test",
        "^test",
        // Boolean operator abuse
        "AND AND AND",
        "OR OR OR",
        "NOT NOT NOT",
        "AND",
        "OR",
        "NOT",
        // Wildcard abuse
        "*",
        "***",
        "* * *",
        "*foo*bar*",
        // Quote abuse
        "\"",
        "\"\"",
        "\"unclosed",
        "\"\"\"\"\"",
        "\"a\" \"b\" \"c\" \"d\"",
        // Hyphen edge cases
        "-",
        "--",
        "---",
        "a-",
        "-b",
        "a-b-c-d-e-f-g-h-i-j-k-l-m-n-o-p",
        "POL-358",
        // Unicode
        "日本語テスト",
        "emoji: 🔥🚀💻",
        "mixed: hello世界",
        "zero-width: a\u{200B}b",
        "rtl: \u{202E}reversed",
        "null: a\0b",
        // Long inputs
        &"a".repeat(10_000),
        &"test ".repeat(1000),
        &"OR ".repeat(500),
        // Special characters
        "\\",
        "\\\\",
        "%",
        "_",
        "\n",
        "\r\n",
        "()",
        "[]",
        "{}",
        "<>",
        "@#$%^&*()",
    ];

    for input in &hostile_inputs {
        // Must not panic
        let result = sanitize_fts_query(input);
        // If it returns Some, the output must not be empty
        if let Some(ref sanitized) = result {
            assert!(
                !sanitized.trim().is_empty(),
                "sanitizer returned non-None but empty for input: {input:?}"
            );
        }
    }
}

/// Plan search must never panic on hostile queries.
#[test]
fn fuzz_planner_no_panic() {
    let hostile_queries = [
        "'; DROP TABLE messages; --",
        "\" OR 1=1",
        &"x".repeat(100_000),
        "\0\0\0",
        "NEAR(a b c d e, 0)",
        &format!("test{}", "\n".repeat(100)),
    ];

    for input in &hostile_queries {
        let q = SearchQuery {
            text: input.to_string(),
            doc_kind: DocKind::Message,
            project_id: Some(1),
            ..Default::default()
        };
        // Must not panic
        let plan = plan_search(&q);
        // Must produce valid SQL (non-empty)
        assert!(!plan.sql.is_empty(), "plan SQL empty for input: {input:?}");
    }
}

/// `extract_like_terms` must safely handle hostile input.
#[test]
fn fuzz_like_terms_safety() {
    let hostile = [
        "",
        " ",
        "AND OR NOT NEAR",
        "a",                 // single char (too short)
        &"x".repeat(50_000), // one very long token
        "a-b c-d e-f g-h i-j k-l",
        "日本語 テスト",
        "\0\0\0",
        "\\%_", // LIKE special chars
    ];

    for input in &hostile {
        let terms = extract_like_terms(input, 10);
        // Must not panic
        for term in &terms {
            // Each term should be at least 2 chars
            assert!(term.len() >= 2, "term too short: {term:?} from {input:?}");
            // No stopwords
            let upper = term.to_ascii_uppercase();
            assert!(
                !["AND", "OR", "NOT", "NEAR"].contains(&upper.as_str()),
                "stopword leaked: {term:?}"
            );
        }
        // Should respect max_terms limit
        assert!(terms.len() <= 10, "exceeded max_terms");
    }
}

/// SQL injection via facet values should be parameterized, not interpolated.
#[test]
fn fuzz_facet_injection_safety() {
    let q = SearchQuery {
        text: "test".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(1),
        thread_id: Some("'; DROP TABLE messages; --".to_string()),
        agent_name: Some("\" OR 1=1 --".to_string()),
        ..Default::default()
    };
    let plan = plan_search(&q);

    // SQL should use parameter placeholders, not interpolated values
    assert!(
        !plan.sql.contains("DROP TABLE"),
        "SQL injection in thread_id: {}",
        plan.sql
    );
    assert!(
        !plan.sql.contains("OR 1=1"),
        "SQL injection in agent_name: {}",
        plan.sql
    );
    // Hostile values should appear in params, not SQL
    assert!(
        plan.params.len() >= 3,
        "hostile facets should be parameterized"
    );
}

/// Cursor with NaN score should not break the planner.
#[test]
fn fuzz_cursor_nan_handling() {
    let nan_cursor = SearchCursor {
        score: f64::NAN,
        id: 42,
    };
    let encoded = nan_cursor.encode();
    let decoded = SearchCursor::decode(&encoded);
    assert!(decoded.is_some(), "NaN cursor should be decodable");
    let dec = decoded.unwrap();
    assert!(
        dec.score.is_nan(),
        "decoded NaN cursor should have NaN score"
    );
    assert_eq!(dec.id, 42);
}

/// Extreme time range values should not cause overflow.
#[test]
fn fuzz_time_range_extremes() {
    let extreme_ranges = [
        TimeRange {
            min_ts: Some(i64::MIN),
            max_ts: Some(i64::MAX),
        },
        TimeRange {
            min_ts: Some(0),
            max_ts: Some(0),
        },
        TimeRange {
            min_ts: Some(i64::MAX),
            max_ts: Some(i64::MIN),
        },
        TimeRange {
            min_ts: Some(-1),
            max_ts: Some(-1),
        },
    ];

    for range in &extreme_ranges {
        let q = SearchQuery {
            doc_kind: DocKind::Message,
            project_id: Some(1),
            time_range: *range,
            ..Default::default()
        };
        // Must not panic
        let plan = plan_search(&q);
        assert!(!plan.sql.is_empty());
    }
}

/// Test that Unicode queries don't corrupt or panic.
#[test]
fn fuzz_unicode_queries() {
    let unicode_inputs = [
        "日本語テスト",
        "Ü̈ber héllo wörld",
        "emoji 🔥 search",
        "مرحبا بالعالم",
        "한국어 검색",
        "\u{FEFF}BOM prefix",
        "combining: e\u{0301}",
        "surrogate-like: \u{FFFD}",
    ];

    for input in &unicode_inputs {
        let sanitized = sanitize_fts_query(input);
        if let Some(ref s) = sanitized {
            // Output must be valid UTF-8 (guaranteed by String but verify no corruption)
            assert!(
                s.is_ascii()
                    || s.chars()
                        .all(|c| c != '\u{FFFD}' || input.contains('\u{FFFD}')),
                "possible corruption for input: {input:?} → {s:?}"
            );
        }

        // Planner must handle it
        let q = SearchQuery::messages(input.to_string(), 1);
        let _plan = plan_search(&q);
    }
}

// ════════════════════════════════════════════════════════════════════
// 3. PARITY: plan ↔ execute consistency
// ════════════════════════════════════════════════════════════════════

/// Verify plan method matches actual execution behavior.
#[test]
fn parity_plan_vs_execute() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    let queries = [
        ("migration", DocKind::Message),
        ("performance", DocKind::Message),
        ("security audit", DocKind::Message),
        ("BlueLake", DocKind::Agent),
        ("parity-test", DocKind::Project),
    ];

    for (text, doc_kind) in &queries {
        let q = SearchQuery {
            text: text.to_string(),
            doc_kind: *doc_kind,
            project_id: if *doc_kind == DocKind::Project {
                None
            } else {
                Some(project_id)
            },
            explain: true,
            ..Default::default()
        };
        let plan = plan_search(&q);
        let pool_c = pool.clone();
        let response: SearchResponse = block_on(|cx| {
            let pool = pool_c;
            async move {
                match execute_search_simple(&cx, &pool, &q).await {
                    Outcome::Ok(r) => r,
                    other => panic!("execute failed for {text:?}: {other:?}"),
                }
            }
        });

        trace_line(
            text,
            plan.method.as_str(),
            plan.normalized_query.as_deref(),
            &plan.facets_applied,
            response.results.len(),
        );

        // Plan method should match explain
        if let Some(ref explain) = response.explain {
            assert_eq!(
                explain.method,
                plan.method.as_str(),
                "plan/explain method mismatch for {text:?}"
            );
        }

        // FTS queries should return results for seeded content
        if plan.method == PlanMethod::Fts && *doc_kind == DocKind::Message {
            assert!(
                !response.results.is_empty(),
                "FTS query {text:?} returned 0 results against seeded corpus"
            );
        }

        // All results should have correct doc_kind
        for result in &response.results {
            assert_eq!(
                result.doc_kind, *doc_kind,
                "result doc_kind mismatch for {text:?}"
            );
        }
    }
}

/// Verify that hostile queries either succeed or fail gracefully (no panics, no data leaks).
#[test]
fn parity_hostile_execute_safety() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    // Some inputs may cause FTS5 syntax errors — that's acceptable as long as
    // there are no panics, no data leaks, and the error is a DB-level error
    // (not a crash or injection).
    let hostile = [
        // These should succeed (sanitizer handles them)
        ("UNION SELECT * FROM sqlite_master", true),
        (&*"test ".repeat(200), true),
        ("*", true),
        // These may cause FTS5 syntax errors (acceptable — sanitizer
        // strips bare single operators but not all operator combinations)
        ("NOT NOT NOT", false),
        ("'; DROP TABLE messages; --", false),
        ("\" OR 1=1", false),
        ("\"unclosed quote", false),
    ];

    for (input, expect_ok) in &hostile {
        let q = SearchQuery::messages(input.to_string(), project_id);
        let plan = plan_search(&q);
        let pool_c = pool.clone();
        let result: Result<SearchResponse, String> = block_on(|cx| {
            let pool = pool_c;
            async move {
                match execute_search_simple(&cx, &pool, &q).await {
                    Outcome::Ok(r) => Ok(r),
                    Outcome::Err(e) => Err(format!("{e:?}")),
                    Outcome::Cancelled(r) => Err(format!("cancelled: {r:?}")),
                    Outcome::Panicked(p) => Err(format!("panicked: {p:?}")),
                }
            }
        });

        trace_line(
            input,
            plan.method.as_str(),
            plan.normalized_query.as_deref(),
            &plan.facets_applied,
            result.as_ref().map_or(0, |r| r.results.len()),
        );

        if *expect_ok {
            assert!(
                result.is_ok(),
                "hostile query {input:?} should succeed but got: {:?}",
                result.err()
            );
        } else {
            // FTS5 syntax error is acceptable for pathological input
            // Just verify it doesn't panic (we got here, so no panic)
            if let Err(ref e) = result {
                assert!(
                    e.contains("syntax error") || e.contains("fts5") || e.contains("Sqlite"),
                    "unexpected error type for {input:?}: {e}"
                );
            }
        }

        // Verify tables still exist after hostile input
        let pool_v = pool.clone();
        let verify: SearchResponse = block_on(|cx| {
            let pool = pool_v;
            async move {
                let q = SearchQuery::messages("migration", project_id);
                match execute_search_simple(&cx, &pool, &q).await {
                    Outcome::Ok(r) => r,
                    other => panic!("post-hostile verification failed: {other:?}"),
                }
            }
        });
        assert!(
            !verify.results.is_empty(),
            "tables should still be intact after hostile input: {input:?}"
        );
    }
}

/// Verify `Fts` and `FilterOnly` produce consistent `project_id` scoping.
#[test]
fn parity_project_scoping() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    // Also seed a second project
    let pool_c = pool.clone();
    let project_id_2: i64 = block_on(|cx| {
        let pool = pool_c;
        async move {
            let p = match queries::ensure_project(&cx, &pool, "/test/other-project").await {
                Outcome::Ok(p) => p,
                other => panic!("ensure other project failed: {other:?}"),
            };
            let pid2 = p.id.unwrap();
            let sender2 = match queries::register_agent(
                &cx,
                &pool,
                pid2,
                "GoldHawk",
                "claude-code",
                "opus-4.6",
                Some("other agent"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a.id.unwrap(),
                other => panic!("register agent2 failed: {other:?}"),
            };
            match queries::register_agent(
                &cx,
                &pool,
                pid2,
                "SilverRidge",
                "claude-code",
                "opus-4.6",
                Some("other recipient"),
                None,
            )
            .await
            {
                Outcome::Ok(_) => {}
                other => panic!("register recip2 failed: {other:?}"),
            }
            // Seed a message in project 2
            match queries::create_message(
                &cx,
                &pool,
                pid2,
                sender2,
                "Cross-project migration test",
                "This message is in project 2 about migration",
                None,
                "normal",
                false,
                "[]",
            )
            .await
            {
                Outcome::Ok(_) => {}
                other => panic!("create p2 msg failed: {other:?}"),
            }
            pid2
        }
    });

    // Search in project 1 only
    let pool_c = pool.clone();
    let p1_results: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery::messages("migration", project_id);
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("p1 search failed: {other:?}"),
            }
        }
    });

    // All results must be from project 1
    for result in &p1_results.results {
        assert_eq!(
            result.project_id,
            Some(project_id),
            "result from wrong project in project-scoped search"
        );
    }

    // Search in project 2
    let pool_c = pool.clone();
    let p2_results: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery::messages("migration", project_id_2);
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("p2 search failed: {other:?}"),
            }
        }
    });

    for result in &p2_results.results {
        assert_eq!(
            result.project_id,
            Some(project_id_2),
            "result from wrong project in p2 search"
        );
    }

    // P2 should have at least 1 result
    assert!(
        !p2_results.results.is_empty(),
        "project 2 should have migration results"
    );
}

/// Verify faceted search filters are actually enforced in results.
#[test]
fn parity_facet_enforcement() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    // Search for urgent messages only
    let pool_c = pool.clone();
    let urgent_results: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery {
                doc_kind: DocKind::Message,
                project_id: Some(project_id),
                importance: vec![Importance::Urgent],
                ..Default::default()
            };
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("urgent search failed: {other:?}"),
            }
        }
    });

    // All returned messages must be urgent
    for result in &urgent_results.results {
        assert_eq!(
            result.importance.as_deref(),
            Some("urgent"),
            "non-urgent result in urgent-only search: {:?}",
            result.title
        );
    }

    // Search for ack_required=true
    let pool_c = pool.clone();
    let ack_results: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery {
                doc_kind: DocKind::Message,
                project_id: Some(project_id),
                ack_required: Some(true),
                ..Default::default()
            };
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("ack search failed: {other:?}"),
            }
        }
    });

    // All returned messages must have ack_required
    for result in &ack_results.results {
        assert_eq!(
            result.ack_required,
            Some(true),
            "non-ack result in ack-required search: {:?}",
            result.title
        );
    }

    // We seeded specific counts: 2 urgent, 6 ack_required
    assert_eq!(
        urgent_results.results.len(),
        2,
        "expected 2 urgent messages"
    );
    assert_eq!(
        ack_results.results.len(),
        6,
        "expected 6 ack_required messages"
    );
}

/// Verify explain output matches plan state after execution.
#[test]
fn parity_explain_consistency() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    let pool_c = pool.clone();
    let mut q = SearchQuery::messages("security", project_id);
    q.explain = true;
    q.importance = vec![Importance::Urgent];

    let plan = plan_search(&q);
    let response: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("explain search failed: {other:?}"),
            }
        }
    });

    let explain = response.explain.expect("explain should be present");

    // Method must match plan
    assert_eq!(explain.method, plan.method.as_str());
    // Facets must match
    assert_eq!(
        explain.facets_applied.len(),
        plan.facets_applied.len(),
        "facet count mismatch: plan={:?} explain={:?}",
        plan.facets_applied,
        explain.facets_applied
    );
    for facet in &plan.facets_applied {
        assert!(
            explain.facets_applied.contains(facet),
            "missing facet in explain: {facet}"
        );
    }
    // SQL must match
    assert_eq!(explain.sql, plan.sql);
}

/// Verify search works across all doc kinds with actual data.
#[test]
fn parity_all_doc_kinds() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender, _recipient) = seed_parity_corpus(&pool);

    // Message search
    let pool_c = pool.clone();
    let msg_resp: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery::messages("database", project_id);
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("msg search failed: {other:?}"),
            }
        }
    });
    assert!(
        !msg_resp.results.is_empty(),
        "message search should find results"
    );
    assert_eq!(msg_resp.results[0].doc_kind, DocKind::Message);

    // Agent search
    let pool_c = pool.clone();
    let agent_resp: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery::agents("RedPeak", project_id);
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("agent search failed: {other:?}"),
            }
        }
    });
    assert!(
        !agent_resp.results.is_empty(),
        "agent search should find RedPeak"
    );
    assert_eq!(agent_resp.results[0].doc_kind, DocKind::Agent);

    // Project search — slug is derived from human_key "/test/parity-conf"
    let pool_c = pool.clone();
    let proj_resp: SearchResponse = block_on(|cx| {
        let pool = pool_c;
        async move {
            let q = SearchQuery::projects("parity");
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("project search failed: {other:?}"),
            }
        }
    });
    assert!(
        !proj_resp.results.is_empty(),
        "project search should find parity project"
    );
    assert_eq!(proj_resp.results[0].doc_kind, DocKind::Project);
}

// ════════════════════════════════════════════════════════════════════
// 4. SANITIZER CONFORMANCE
// ════════════════════════════════════════════════════════════════════

/// Deterministic sanitizer conformance vectors.
#[test]
fn sanitizer_conformance_vectors() {
    let vectors: Vec<(&str, Option<&str>)> = vec![
        // Basic pass-through
        ("hello world", Some("hello world")),
        ("migration", Some("migration")),
        // Operators stripped
        ("AND", None),
        ("OR", None),
        ("NOT", None),
        // Empty
        ("", None),
        ("   ", None),
        // Wildcards
        ("*", None),
        ("*foo", Some("foo")),
        ("**foo", Some("foo")),
        ("foo *", Some("foo")),
        // Hyphenated tokens get quoted
        ("POL-358", Some("\"POL-358\"")),
        ("foo-bar-baz", Some("\"foo-bar-baz\"")),
        // Already quoted
        ("\"hello world\"", Some("\"hello world\"")),
        // Whitespace collapse
        ("hello  world", Some("hello world")),
        ("a   b   c", Some("a b c")),
    ];

    for (input, expected) in &vectors {
        let result = sanitize_fts_query(input);
        let result_str = result.as_deref();
        assert_eq!(
            result_str, *expected,
            "sanitize({input:?}): expected {expected:?}, got {result_str:?}"
        );
    }
}

/// `extract_like_terms` conformance vectors.
#[test]
fn like_terms_conformance_vectors() {
    let vectors: Vec<(&str, usize, Vec<&str>)> = vec![
        ("hello world", 10, vec!["hello", "world"]),
        ("AND OR NOT", 10, vec![]),
        ("a b cc dd", 10, vec!["cc", "dd"]),
        ("hello hello hello", 10, vec!["hello"]),
        ("POL-358 migration", 10, vec!["POL-358", "migration"]),
        ("test", 1, vec!["test"]),
        ("", 10, vec![]),
    ];

    for (input, max, expected) in &vectors {
        let result = extract_like_terms(input, *max);
        let result_strs: Vec<&str> = result.iter().map(String::as_str).collect();
        assert_eq!(
            result_strs, *expected,
            "extract_like_terms({input:?}, {max}): expected {expected:?}, got {result_strs:?}"
        );
    }
}
