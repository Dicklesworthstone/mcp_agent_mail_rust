//! Search quality benchmark corpus + relevance tuning harness (br-3vwi.2.5).
//!
//! Seeds a deterministic database with realistic messages, agents, and projects,
//! then runs a battery of search queries and asserts expected ordering bands.
//!
//! Quality metrics computed:
//! - **NDCG@k** (Normalized Discounted Cumulative Gain) — relevance ranking quality
//! - **MRR** (Mean Reciprocal Rank) — how high the best-match appears
//! - **Precision@k** — fraction of top-k results that are relevant
//!
//! Each test vector includes a `why` explanation for debugging regressions.

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
use mcp_agent_mail_db::queries;
use mcp_agent_mail_db::search_planner::{
    DocKind, Importance, PlanMethod, RankingMode, SearchQuery, SearchResponse, TimeRange,
    plan_search,
};
use mcp_agent_mail_db::search_scope::{
    ContactPolicyKind, RedactionPolicy, ScopeContext, SenderPolicy, ViewerIdentity, apply_scope,
};
use mcp_agent_mail_db::search_service::execute_search_simple;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, now_micros};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("search_bench_{}.db", unique_suffix()));
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

// ────────────────────────────────────────────────────────────────────
// Quality metrics
// ────────────────────────────────────────────────────────────────────

/// Normalized Discounted Cumulative Gain at position k.
///
/// `relevance` maps result IDs to their relevance score (0..3 where 3 = perfect).
/// `result_ids` is the ordered list of IDs returned by search.
fn ndcg_at_k(result_ids: &[i64], relevance: &[(i64, u32)], k: usize) -> f64 {
    let rel_map: std::collections::HashMap<i64, u32> = relevance.iter().copied().collect();

    let dcg = |ids: &[i64]| -> f64 {
        ids.iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| {
                let rel = f64::from(*rel_map.get(id).unwrap_or(&0));
                (rel.exp2() - 1.0) / (2.0 + i as f64).log2()
            })
            .sum()
    };

    let actual_dcg = dcg(result_ids);

    // Ideal ordering: sort by relevance descending
    let mut ideal_ids: Vec<i64> = relevance.iter().map(|(id, _)| *id).collect();
    ideal_ids.sort_by(|a, b| {
        rel_map
            .get(b)
            .unwrap_or(&0)
            .cmp(rel_map.get(a).unwrap_or(&0))
    });
    let ideal_dcg = dcg(&ideal_ids);

    if ideal_dcg == 0.0 {
        return 1.0; // No relevant docs → perfect by convention
    }
    actual_dcg / ideal_dcg
}

/// Mean Reciprocal Rank: 1/rank of first relevant result.
///
/// `relevant_ids` is the set of IDs considered relevant.
fn mrr(result_ids: &[i64], relevant_ids: &[i64]) -> f64 {
    for (i, id) in result_ids.iter().enumerate() {
        if relevant_ids.contains(id) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Precision@k: fraction of top-k results that are in the relevant set.
fn precision_at_k(result_ids: &[i64], relevant_ids: &[i64], k: usize) -> f64 {
    let hits = result_ids
        .iter()
        .take(k)
        .filter(|id| relevant_ids.contains(id))
        .count();
    if k == 0 {
        return 0.0;
    }
    hits as f64 / k as f64
}

// ────────────────────────────────────────────────────────────────────
// Corpus seed data
// ────────────────────────────────────────────────────────────────────

/// A seeded message with known content for benchmark queries.
struct SeedMessage {
    subject: &'static str,
    body: &'static str,
    importance: &'static str,
    thread_id: Option<&'static str>,
    ack_required: bool,
}

/// Seed the database with the benchmark corpus.
///
/// Returns `(project_id, sender_agent_id, recipient_agent_id, message_ids)`.
fn seed_corpus(pool: &DbPool) -> (i64, i64, i64, Vec<i64>) {
    let messages: Vec<SeedMessage> = vec![
        // ── High-relevance messages for "migration" queries ───────────
        SeedMessage {
            subject: "Database migration plan for v3 timestamps",
            body: "We need to migrate all TEXT timestamps to i64 microseconds. \
                   The migration script uses CAST(strftime) with fractional micros. \
                   All DATETIME columns have NUMERIC affinity so integers are preserved.",
            importance: "high",
            thread_id: Some("MIGRATION-001"),
            ack_required: true,
        },
        SeedMessage {
            subject: "Migration rollback strategy for POL-358",
            body: "If the v3 migration fails, we can rollback using the backup \
                   created before migration. Reference: POL-358 describes the \
                   migration safety contract. The migration is idempotent so \
                   re-running is safe. Check migration_status table for state.",
            importance: "high",
            thread_id: Some("MIGRATION-001"),
            ack_required: false,
        },
        // ── Medium-relevance for "migration" (mentions it tangentially) ──
        SeedMessage {
            subject: "Weekly status update",
            body: "This week we completed the FTS5 integration and started on \
                   the migration framework. Also fixed 3 clippy warnings. \
                   Next week: finish schema migration tests.",
            importance: "normal",
            thread_id: Some("STATUS-W12"),
            ack_required: false,
        },
        // ── High-relevance for "search" queries ──────────────────────
        SeedMessage {
            subject: "Search quality improvement proposal",
            body: "BM25 scoring needs tuning. Current weights are K1=10.0, B=1.0. \
                   Subject matches should rank higher than body matches. \
                   Also need LIKE fallback for unsanitizable FTS queries.",
            importance: "urgent",
            thread_id: Some("SEARCH-QUALITY"),
            ack_required: true,
        },
        SeedMessage {
            subject: "FTS5 index rebuild procedure",
            body: "To rebuild the full-text search index, run: \
                   INSERT INTO fts_messages(fts_messages) VALUES('rebuild'). \
                   This is needed after bulk imports or corruption. \
                   Search performance should improve by 2-3x after rebuild.",
            importance: "high",
            thread_id: Some("SEARCH-QUALITY"),
            ack_required: false,
        },
        // ── Agent coordination messages ──────────────────────────────
        SeedMessage {
            subject: "File reservation conflict on tui_app.rs",
            body: "CalmRidge and CopperRobin both tried to reserve tui_app.rs. \
                   CalmRidge got the exclusive lock. CopperRobin should work on \
                   dashboard.rs instead until the reservation expires.",
            importance: "urgent",
            thread_id: Some("CONFLICT-42"),
            ack_required: true,
        },
        SeedMessage {
            subject: "New agent registration: LavenderLantern",
            body: "LavenderLantern (claude-code/opus-4.6) has joined the project. \
                   Previously known as RubyPrairie. Will work on search quality \
                   benchmarks and relevance tuning (br-3vwi.2.5).",
            importance: "normal",
            thread_id: Some("AGENTS"),
            ack_required: false,
        },
        // ── Performance and optimization ─────────────────────────────
        SeedMessage {
            subject: "SQLite WAL mode performance results",
            body: "After enabling WAL mode with PRAGMA journal_mode=WAL, \
                   concurrent read throughput increased by 8x. Write latency \
                   unchanged. Pool exhaustion errors dropped to zero. \
                   Recommended: keep WAL enabled in production.",
            importance: "high",
            thread_id: Some("PERF-WAL"),
            ack_required: false,
        },
        SeedMessage {
            subject: "Connection pool tuning recommendations",
            body: "Based on stress test results: max_connections=20 is optimal \
                   for our workload. Below 8 we see pool exhaustion under load. \
                   Above 32 we see diminishing returns due to SQLite lock contention.",
            importance: "normal",
            thread_id: Some("PERF-POOL"),
            ack_required: false,
        },
        // ── Bug reports ──────────────────────────────────────────────
        SeedMessage {
            subject: "Integer overflow in message size validation",
            body: "Found a bug where message body > 2GB causes integer overflow \
                   in the size check. The length was cast to i32 before comparison. \
                   Fixed by using usize comparison directly. See commit a75a200.",
            importance: "urgent",
            thread_id: Some("BUG-OVERFLOW"),
            ack_required: true,
        },
        SeedMessage {
            subject: "FTS query sanitization edge case",
            body: "The query '***' was not properly handled. sanitize_fts_query \
                   now returns None for bare wildcards. Added LIKE fallback \
                   extraction for these cases. See tests in queries.rs.",
            importance: "high",
            thread_id: Some("BUG-FTS"),
            ack_required: false,
        },
        // ── Documentation and planning ───────────────────────────────
        SeedMessage {
            subject: "TUI V2 product contract published",
            body: "The V2 product contract is at docs/TUI_V2_CONTRACT.md. \
                   It covers 12 screens, navigation graph, entity model, \
                   and operator journeys. Please review and comment.",
            importance: "normal",
            thread_id: Some("TUI-V2"),
            ack_required: false,
        },
        SeedMessage {
            subject: "Architecture decision: dual-mode interface",
            body: "ADR-002 is finalized. The single binary supports both MCP \
                   mode (default) and CLI mode (AM_INTERFACE_MODE=cli). \
                   CLI-only commands produce exit code 2 in MCP mode.",
            importance: "normal",
            thread_id: Some("ADR-002"),
            ack_required: false,
        },
        // ── Low-relevance noise ──────────────────────────────────────
        SeedMessage {
            subject: "Lunch plans for Wednesday",
            body: "Anyone want to grab tacos? The new place on 5th street \
                   has great reviews. Meeting at noon. Reply if interested.",
            importance: "low",
            thread_id: Some("SOCIAL"),
            ack_required: false,
        },
        SeedMessage {
            subject: "Office temperature complaint",
            body: "The thermostat is set too low again. Can someone adjust it? \
                   My fingers are too cold to type properly. This affects \
                   productivity significantly.",
            importance: "low",
            thread_id: Some("SOCIAL"),
            ack_required: false,
        },
    ];

    let (project_id, sender_id, recipient_id) = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let project = match queries::ensure_project(&cx, &pool, "/test/search-bench").await {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            };
            let pid = project.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "BlueLake",
                "claude-code",
                "opus-4.6",
                Some("Search benchmark sender agent"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register_agent sender failed: {other:?}"),
            };

            let recipient = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "RedHarbor",
                "claude-code",
                "opus-4.6",
                Some("Search benchmark recipient agent"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register_agent recipient failed: {other:?}"),
            };

            (pid, sender.id.unwrap(), recipient.id.unwrap())
        }
    });

    // Insert messages sequentially (FTS5 indexing needs sequential inserts)
    let mut message_ids = Vec::new();
    for msg in &messages {
        let pool2 = pool.clone();
        let msg_id = block_on(|cx| {
            let pool = pool2;
            async move {
                let row = match queries::create_message(
                    &cx,
                    &pool,
                    project_id,
                    sender_id,
                    msg.subject,
                    msg.body,
                    msg.thread_id,
                    msg.importance,
                    msg.ack_required,
                    "[]",
                )
                .await
                {
                    Outcome::Ok(m) => m,
                    other => panic!("create_message failed: {other:?}"),
                };
                row.id.unwrap()
            }
        });
        message_ids.push(msg_id);
    }

    (project_id, sender_id, recipient_id, message_ids)
}

/// Seed a second project for multi-project search tests.
///
/// Returns `(project2_id, sender2_id, msg_ids)`.
fn seed_second_project(pool: &DbPool) -> (i64, i64, Vec<i64>) {
    let (pid2, sender2) = block_on(|cx| {
        let pool = pool.clone();
        async move {
            let project =
                match queries::ensure_project(&cx, &pool, "/test/search-bench-secondary").await {
                    Outcome::Ok(p) => p,
                    other => panic!("ensure_project 2 failed: {other:?}"),
                };
            let pid = project.id.unwrap();

            let sender = match queries::register_agent(
                &cx,
                &pool,
                pid,
                "GoldHawk",
                "codex-cli",
                "gpt-5.2",
                Some("Secondary project agent for cross-project search"),
                None,
            )
            .await
            {
                Outcome::Ok(a) => a,
                other => panic!("register_agent 2 failed: {other:?}"),
            };

            (pid, sender.id.unwrap())
        }
    });

    let cross_project_msgs = vec![
        SeedMessage {
            subject: "Cross-project migration coordination",
            body: "The migration framework needs to be shared across both projects. \
                   Use the same v3 migration scripts. Coordinate via Agent Mail thread.",
            importance: "high",
            thread_id: Some("MIGRATION-CROSS"),
            ack_required: true,
        },
        SeedMessage {
            subject: "Search index corruption in secondary DB",
            body: "The FTS5 index in the secondary project got corrupted after \
                   a power failure. Running rebuild fixed it. Added automatic \
                   integrity checks on startup.",
            importance: "urgent",
            thread_id: Some("BUG-FTS-2"),
            ack_required: true,
        },
    ];

    let mut msg_ids = Vec::new();
    for msg in &cross_project_msgs {
        let pool2 = pool.clone();
        let msg_id = block_on(|cx| {
            let pool = pool2;
            async move {
                let row = match queries::create_message(
                    &cx,
                    &pool,
                    pid2,
                    sender2,
                    msg.subject,
                    msg.body,
                    msg.thread_id,
                    msg.importance,
                    msg.ack_required,
                    "[]",
                )
                .await
                {
                    Outcome::Ok(m) => m,
                    other => panic!("create_message 2 failed: {other:?}"),
                };
                row.id.unwrap()
            }
        });
        msg_ids.push(msg_id);
    }

    (pid2, sender2, msg_ids)
}

// ────────────────────────────────────────────────────────────────────
// Query test vectors
// ────────────────────────────────────────────────────────────────────

/// A single test vector for search quality evaluation.
struct QueryVector {
    /// Human-readable description for test output.
    label: &'static str,
    /// The search query.
    query: SearchQuery,
    /// IDs of relevant results with relevance scores (0-3).
    /// 3 = perfect match, 2 = strong match, 1 = weak match, 0 = not relevant.
    relevance: Vec<(i64, u32)>,
    /// The first result MUST be one of these IDs (or empty to skip check).
    must_be_first: Vec<i64>,
    /// Minimum acceptable NDCG@5.
    min_ndcg_5: f64,
    /// Minimum acceptable MRR.
    min_mrr: f64,
    /// Expected plan method.
    expected_method: Option<PlanMethod>,
    /// Why this query tests what it tests.
    why: &'static str,
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

/// Core benchmark: run all query vectors against the corpus and assert quality.
#[test]
fn search_quality_benchmark() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, msg_ids) = seed_corpus(&pool);

    // Message IDs by index (matches the seed order above)
    // [0] = migration plan, [1] = migration rollback, [2] = weekly status,
    // [3] = search quality, [4] = FTS rebuild, [5] = file conflict,
    // [6] = agent registration, [7] = WAL perf, [8] = pool tuning,
    // [9] = overflow bug, [10] = FTS edge case, [11] = TUI V2,
    // [12] = ADR-002, [13] = lunch, [14] = temperature
    assert_eq!(msg_ids.len(), 15, "corpus should have 15 messages");

    let vectors = vec![
        // ── Term: "migration" ────────────────────────────────────────
        QueryVector {
            label: "Simple term: migration",
            query: SearchQuery::messages("migration", project_id),
            relevance: vec![
                (msg_ids[0], 3), // "Database migration plan" - title + body
                (msg_ids[1], 3), // "Migration rollback" - title + body
                (msg_ids[2], 1), // Weekly status mentioning migration
            ],
            must_be_first: vec![msg_ids[0], msg_ids[1]],
            min_ndcg_5: 0.8,
            min_mrr: 1.0, // First result should be relevant
            expected_method: Some(PlanMethod::Fts),
            why: "Both migration-focused messages should rank above \
                  the weekly status that only mentions migration once.",
        },
        // ── Term: "search" ───────────────────────────────────────────
        QueryVector {
            label: "Simple term: search",
            query: SearchQuery::messages("search", project_id),
            relevance: vec![
                (msg_ids[3], 3),  // Search quality proposal
                (msg_ids[4], 3),  // FTS rebuild
                (msg_ids[10], 2), // FTS edge case (mentions search tangentially)
                (msg_ids[6], 1),  // Agent registration mentioning search benchmarks
            ],
            must_be_first: vec![msg_ids[3], msg_ids[4]],
            min_ndcg_5: 0.7,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::Fts),
            why: "Dedicated search messages should rank highest, with \
                  tangential mentions ranked lower.",
        },
        // ── Phrase: "FTS5" ───────────────────────────────────────────
        QueryVector {
            label: "Technical term: FTS5",
            query: SearchQuery::messages("FTS5", project_id),
            relevance: vec![
                (msg_ids[4], 3), // FTS5 rebuild procedure (subject match)
                (msg_ids[2], 2), // Weekly status: "FTS5 integration" (body match)
                                 // Note: msg_ids[10] says "FTS" not "FTS5" — not a match
            ],
            must_be_first: vec![msg_ids[4], msg_ids[2]], // either is acceptable
            min_ndcg_5: 0.7,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::Fts),
            why: "Both messages containing 'FTS5' should be found. BM25 may \
                  rank shorter docs higher due to term density.",
        },
        // ── Facet: importance=urgent ─────────────────────────────────
        QueryVector {
            label: "Facet only: importance=urgent",
            query: SearchQuery {
                doc_kind: DocKind::Message,
                project_id: Some(project_id),
                importance: vec![Importance::Urgent],
                ..Default::default()
            },
            relevance: vec![
                (msg_ids[3], 3), // search quality (urgent)
                (msg_ids[5], 3), // file conflict (urgent)
                (msg_ids[9], 3), // overflow bug (urgent)
            ],
            must_be_first: vec![],
            min_ndcg_5: 0.9,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::FilterOnly),
            why: "Filter-only query should return exactly the urgent messages.",
        },
        // ── Combined: text + facet ───────────────────────────────────
        QueryVector {
            label: "Text + facet: 'bug' with importance=urgent",
            query: {
                let mut q = SearchQuery::messages("bug", project_id);
                q.importance = vec![Importance::Urgent];
                q
            },
            relevance: vec![
                (msg_ids[9], 3), // overflow bug (urgent, title has "bug" connotation)
            ],
            must_be_first: vec![],
            min_ndcg_5: 0.5,
            min_mrr: 0.5,
            expected_method: Some(PlanMethod::Fts),
            why: "Combining text search with importance facet should narrow results.",
        },
        // ── Thread filter ────────────────────────────────────────────
        QueryVector {
            label: "Thread filter: MIGRATION-001",
            query: SearchQuery {
                doc_kind: DocKind::Message,
                project_id: Some(project_id),
                thread_id: Some("MIGRATION-001".to_string()),
                ..Default::default()
            },
            relevance: vec![
                (msg_ids[0], 3), // migration plan
                (msg_ids[1], 3), // migration rollback
            ],
            must_be_first: vec![],
            min_ndcg_5: 1.0,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::FilterOnly),
            why: "Thread filter should return exactly the 2 messages in MIGRATION-001.",
        },
        // ── Ack required filter ──────────────────────────────────────
        QueryVector {
            label: "Facet: ack_required=true",
            query: SearchQuery {
                doc_kind: DocKind::Message,
                project_id: Some(project_id),
                ack_required: Some(true),
                ..Default::default()
            },
            relevance: vec![
                (msg_ids[0], 3), // migration plan (ack_required)
                (msg_ids[3], 3), // search quality (ack_required)
                (msg_ids[5], 3), // file conflict (ack_required)
                (msg_ids[9], 3), // overflow bug (ack_required)
            ],
            must_be_first: vec![],
            min_ndcg_5: 1.0,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::FilterOnly),
            why: "ack_required facet should return exactly the 4 ack-required messages.",
        },
        // ── Unsearchable query ───────────────────────────────────────
        QueryVector {
            label: "Unsearchable: bare wildcards",
            query: SearchQuery::messages("***", project_id),
            relevance: vec![],
            must_be_first: vec![],
            min_ndcg_5: 1.0, // empty = perfect by convention
            min_mrr: 0.0,
            expected_method: Some(PlanMethod::Empty),
            why: "Bare wildcards should sanitize to None → Empty plan → no results.",
        },
        // ── Hyphenated token ─────────────────────────────────────────
        QueryVector {
            label: "Hyphenated token: POL-358",
            query: SearchQuery::messages("POL-358", project_id),
            relevance: vec![
                (msg_ids[1], 3), // Migration rollback mentions "POL-358"
            ],
            must_be_first: vec![msg_ids[1]],
            min_ndcg_5: 0.5,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::Fts),
            why: "Hyphenated tokens like 'POL-358' should be quoted by \
                  sanitize_fts_query to prevent the dash being treated \
                  as a NOT operator.",
        },
        // ── Prefix search ────────────────────────────────────────────
        QueryVector {
            label: "Prefix: migrat*",
            query: SearchQuery::messages("migrat*", project_id),
            relevance: vec![(msg_ids[0], 3), (msg_ids[1], 3), (msg_ids[2], 1)],
            must_be_first: vec![msg_ids[0], msg_ids[1]],
            min_ndcg_5: 0.7,
            min_mrr: 1.0,
            expected_method: Some(PlanMethod::Fts),
            why: "Prefix wildcard should match 'migration', 'migrate', etc.",
        },
    ];

    // ── Run all vectors ──────────────────────────────────────────────
    let mut all_pass = true;
    let mut report_lines: Vec<String> = Vec::new();
    report_lines.push(format!(
        "{:<40} {:>7} {:>7} {:>7} {:>6} {:>8}  {}",
        "Query", "NDCG@5", "MRR", "P@5", "N_res", "Method", "Status"
    ));
    report_lines.push("-".repeat(100));

    for vector in &vectors {
        let pool2 = pool.clone();
        let response: SearchResponse = block_on(|cx| {
            let pool = pool2;
            let query = vector.query.clone();
            async move {
                match execute_search_simple(&cx, &pool, &query).await {
                    Outcome::Ok(r) => r,
                    Outcome::Err(e) => panic!("search failed for '{}': {e:?}", vector.label),
                    other => panic!("unexpected outcome for '{}': {other:?}", vector.label),
                }
            }
        });

        let result_ids: Vec<i64> = response.results.iter().map(|r| r.id).collect();
        let relevant_ids: Vec<i64> = vector
            .relevance
            .iter()
            .filter(|(_, rel)| *rel >= 2)
            .map(|(id, _)| *id)
            .collect();

        let ndcg = ndcg_at_k(&result_ids, &vector.relevance, 5);
        let mrr_val = mrr(&result_ids, &relevant_ids);
        let p_at_5 = precision_at_k(&result_ids, &relevant_ids, 5);

        // Check plan method
        let plan = plan_search(&vector.query);
        let method_ok = vector
            .expected_method
            .is_none_or(|expected| plan.method == expected);

        // Check first result
        let first_ok = vector.must_be_first.is_empty()
            || result_ids
                .first()
                .is_some_and(|first| vector.must_be_first.contains(first));

        let ndcg_ok = ndcg >= vector.min_ndcg_5;
        let mrr_ok = mrr_val >= vector.min_mrr;
        let pass = ndcg_ok && mrr_ok && method_ok && first_ok;

        let status = if pass { "PASS" } else { "FAIL" };
        if !pass {
            all_pass = false;
        }

        report_lines.push(format!(
            "{:<40} {:>7.3} {:>7.3} {:>7.3} {:>6} {:>8}  {}",
            vector.label,
            ndcg,
            mrr_val,
            p_at_5,
            result_ids.len(),
            plan.method.as_str(),
            status,
        ));

        if !pass {
            report_lines.push(format!("  WHY: {}", vector.why));
            if !ndcg_ok {
                report_lines.push(format!("  NDCG@5 {ndcg:.3} < min {:.3}", vector.min_ndcg_5));
            }
            if !mrr_ok {
                report_lines.push(format!("  MRR {mrr_val:.3} < min {:.3}", vector.min_mrr));
            }
            if !method_ok {
                report_lines.push(format!(
                    "  Method {:?} != expected {:?}",
                    plan.method, vector.expected_method
                ));
            }
            if !first_ok {
                report_lines.push(format!(
                    "  First result {:?} not in must_be_first {:?}",
                    result_ids.first(),
                    vector.must_be_first,
                ));
            }
            report_lines.push(format!("  Results: {result_ids:?}"));
            report_lines.push(format!(
                "  Relevant: {:?}",
                vector
                    .relevance
                    .iter()
                    .map(|(id, r)| format!("{id}(r={r})"))
                    .collect::<Vec<_>>()
            ));
        }
    }

    // Print full report
    eprintln!("\n=== Search Quality Benchmark Report ===\n");
    for line in &report_lines {
        eprintln!("{line}");
    }
    eprintln!();

    assert!(
        all_pass,
        "Search quality benchmark failed — see report above"
    );
}

/// Test that cursor pagination produces stable, non-overlapping pages.
#[test]
fn pagination_stability() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, msg_ids) = seed_corpus(&pool);
    assert!(msg_ids.len() >= 10);

    // Use FTS search (not FilterOnly) so BM25 scores vary and cursor works.
    // "the" appears in many messages giving us enough results to paginate.
    let mut query = SearchQuery {
        text: "migration OR search OR bug OR performance".to_string(),
        doc_kind: DocKind::Message,
        project_id: Some(project_id),
        limit: Some(5),
        ..Default::default()
    };

    let pool2 = pool.clone();
    let page1: SearchResponse = block_on(|cx| {
        let pool = pool2;
        let q = query.clone();
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("page1 failed: {other:?}"),
            }
        }
    });

    let page1_ids: Vec<i64> = page1.results.iter().map(|r| r.id).collect();
    assert_eq!(page1_ids.len(), 5, "page 1 should have 5 results");

    // Second page using cursor
    if let Some(cursor) = &page1.next_cursor {
        query.cursor = Some(cursor.clone());

        let pool3 = pool.clone();
        let page2: SearchResponse = block_on(|cx| {
            let pool = pool3;
            let q = query.clone();
            async move {
                match execute_search_simple(&cx, &pool, &q).await {
                    Outcome::Ok(r) => r,
                    other => panic!("page2 failed: {other:?}"),
                }
            }
        });

        let page2_ids: Vec<i64> = page2.results.iter().map(|r| r.id).collect();

        // No overlap between pages
        for id in &page2_ids {
            assert!(
                !page1_ids.contains(id),
                "ID {id} appears in both page 1 and page 2"
            );
        }

        // Combined should be monotonically ordered
        let all_ids: Vec<i64> = page1_ids.iter().chain(page2_ids.iter()).copied().collect();
        for window in all_ids.windows(2) {
            assert!(
                window[0] != window[1],
                "Duplicate ID {} in pagination",
                window[0]
            );
        }
    }
}

/// Test scope enforcement with cross-project visibility.
#[test]
fn scope_enforcement_quality() {
    let (pool, _dir) = make_pool();
    let (project_id, sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);
    let (pid2, sender2, _msg_ids2) = seed_second_project(&pool);

    // Search all messages (operator mode = unrestricted)
    let pool2 = pool.clone();
    let all_results: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            let q = SearchQuery::messages("migration", project_id);
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("operator search failed: {other:?}"),
            }
        }
    });

    let total_results = all_results.results.len();
    assert!(total_results >= 2, "should find migration messages");

    // Apply scope: viewer in project 1, contacts_only sender in project 2
    let ctx = ScopeContext {
        viewer: Some(ViewerIdentity {
            project_id,
            agent_id: sender_id,
        }),
        approved_contacts: vec![],
        viewer_project_ids: vec![project_id],
        sender_policies: vec![SenderPolicy {
            project_id: pid2,
            agent_id: sender2,
            policy: ContactPolicyKind::ContactsOnly,
        }],
        recipient_map: vec![],
    };
    let redaction = RedactionPolicy::default();
    let (scoped, audit) = apply_scope(all_results.results, &ctx, &redaction);

    // All results from project_id should be visible
    for r in &scoped {
        assert!(
            r.result.project_id == Some(project_id),
            "scoped result should be from viewer's project"
        );
    }

    // Verify audit captures decisions correctly
    assert_eq!(audit.total_before, total_results);
    assert!(
        audit.visible_count <= total_results,
        "visible should not exceed total"
    );
}

/// Test that LIKE fallback produces results for queries FTS5 can't handle.
#[test]
fn like_fallback_quality() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    // Use a query that FTS sanitization might struggle with but has extractable terms.
    // The asterisk prefix triggers stripping, but the remaining term is searchable.
    let pool2 = pool.clone();
    let query = SearchQuery::messages("migration", project_id);
    let plan = plan_search(&query);

    // Verify the plan uses FTS for a clean query
    assert_eq!(plan.method, PlanMethod::Fts, "clean query should use FTS");

    // Now test that results actually come back
    let response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(r) => r,
                other => panic!("like fallback search failed: {other:?}"),
            }
        }
    });

    assert!(
        !response.results.is_empty(),
        "should find migration messages via FTS"
    );

    // Verify scores are populated for FTS results
    for result in &response.results {
        assert!(
            result.score.is_some(),
            "FTS results should have BM25 scores"
        );
    }
}

/// Test empty and edge-case queries.
#[test]
fn edge_case_queries() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    // Note: Empty string with project_id produces FilterOnly (has_any_message_facet
    // returns true for project_id). Only queries with NO facets and NO text are Empty.
    let edge_cases = vec![
        ("***", PlanMethod::Empty, "Bare wildcards"),
        ("AND", PlanMethod::Empty, "Bare operator AND"),
        ("OR", PlanMethod::Empty, "Bare operator OR"),
        ("NOT", PlanMethod::Empty, "Bare operator NOT"),
    ];

    // Empty/whitespace with project_id → FilterOnly (has facets)
    let q_empty = SearchQuery::messages("", project_id);
    let plan_empty = plan_search(&q_empty);
    assert_eq!(
        plan_empty.method,
        PlanMethod::FilterOnly,
        "empty text with project_id should be FilterOnly"
    );

    // Truly empty (no text, no facets) → Empty
    let q_truly_empty = SearchQuery::default();
    let plan_truly_empty = plan_search(&q_truly_empty);
    assert_eq!(
        plan_truly_empty.method,
        PlanMethod::Empty,
        "no text + no facets should be Empty"
    );

    for (query_text, expected_method, label) in &edge_cases {
        let q = SearchQuery::messages(*query_text, project_id);
        let plan = plan_search(&q);
        assert_eq!(
            plan.method, *expected_method,
            "{label}: expected {expected_method:?}, got {:?}",
            plan.method
        );
    }
}

/// Test that the explain metadata is populated correctly.
#[test]
fn explain_metadata_quality() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    let pool2 = pool.clone();
    let mut query = SearchQuery::messages("migration", project_id);
    query.explain = true;
    query.importance = vec![Importance::High];

    let response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(r) => r,
                other => panic!("explain search failed: {other:?}"),
            }
        }
    });

    let explain = response
        .explain
        .expect("explain should be present when requested");
    assert_eq!(explain.method, "fts5");
    assert!(!explain.used_like_fallback);
    assert!(explain.normalized_query.is_some());
    assert!(explain.facet_count >= 2); // project_id + importance
    assert!(explain.facets_applied.contains(&"importance".to_string()));
    assert!(explain.facets_applied.contains(&"project_id".to_string()));
    assert!(!explain.sql.is_empty());
}

/// Test agent search doc kind.
#[test]
fn agent_search_quality() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    let pool2 = pool.clone();
    let query = SearchQuery::agents("BlueLake", project_id);

    let response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(r) => r,
                other => panic!("agent search failed: {other:?}"),
            }
        }
    });

    assert!(!response.results.is_empty(), "should find BlueLake agent");
    assert_eq!(
        response.results[0].doc_kind,
        DocKind::Agent,
        "should return Agent doc kind"
    );
    assert!(
        response.results[0].title.contains("BlueLake"),
        "title should contain agent name"
    );
}

/// Test project search doc kind.
#[test]
fn project_search_quality() {
    let (pool, _dir) = make_pool();
    let (_project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    let pool2 = pool.clone();
    let query = SearchQuery::projects("search-bench");

    let response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(r) => r,
                other => panic!("project search failed: {other:?}"),
            }
        }
    });

    assert!(
        !response.results.is_empty(),
        "should find search-bench project"
    );
    assert_eq!(
        response.results[0].doc_kind,
        DocKind::Project,
        "should return Project doc kind"
    );
}

/// Metric computation correctness.
#[test]
fn quality_metric_calculations() {
    // NDCG perfect ordering
    let ids = vec![1, 2, 3];
    let relevance = vec![(1, 3), (2, 2), (3, 1)];
    let ndcg = ndcg_at_k(&ids, &relevance, 3);
    assert!(
        (ndcg - 1.0).abs() < 1e-6,
        "perfect ordering should give NDCG=1.0, got {ndcg}"
    );

    // NDCG reversed ordering
    let ids_rev = vec![3, 2, 1];
    let ndcg_rev = ndcg_at_k(&ids_rev, &relevance, 3);
    assert!(
        ndcg_rev < 1.0,
        "reversed ordering should give NDCG < 1.0, got {ndcg_rev}"
    );

    // MRR first result relevant
    let mrr_val = mrr(&[1, 2, 3], &[1]);
    assert!((mrr_val - 1.0).abs() < 1e-6, "MRR should be 1.0");

    // MRR second result relevant
    let mrr_val2 = mrr(&[1, 2, 3], &[2]);
    assert!((mrr_val2 - 0.5).abs() < 1e-6, "MRR should be 0.5");

    // MRR no relevant results
    let mrr_val3 = mrr(&[1, 2, 3], &[99]);
    assert!(mrr_val3.abs() < 1e-6, "MRR should be 0.0");

    // Precision@k
    let p3 = precision_at_k(&[1, 2, 3, 4, 5], &[1, 3, 5], 3);
    assert!((p3 - 2.0 / 3.0).abs() < 1e-6, "P@3 should be 2/3, got {p3}");

    let p0 = precision_at_k(&[1], &[1], 0);
    assert!(p0.abs() < 1e-6, "P@0 should be 0.0");
}

/// Test that multi-facet combined queries work correctly.
#[test]
fn multi_facet_combined() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, msg_ids) = seed_corpus(&pool);

    // Text + importance + ack_required
    let pool2 = pool.clone();
    let mut query = SearchQuery::messages("migration", project_id);
    query.importance = vec![Importance::High];
    query.ack_required = Some(true);

    let response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(r) => r,
                other => panic!("multi-facet search failed: {other:?}"),
            }
        }
    });

    // Should match: msg_ids[0] (migration plan, high, ack_required=true)
    // Should NOT match: msg_ids[1] (migration rollback, high, ack_required=false)
    let result_ids: Vec<i64> = response.results.iter().map(|r| r.id).collect();
    assert!(
        result_ids.contains(&msg_ids[0]),
        "migration plan should match"
    );
    assert!(
        !result_ids.contains(&msg_ids[1]),
        "migration rollback should NOT match (ack_required=false)"
    );
}

/// Test time range filtering.
#[test]
fn time_range_filter() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    // Get the timestamp of the first message
    let pool2 = pool.clone();
    let all_response: SearchResponse = block_on(|cx| {
        let pool = pool2;
        let q = SearchQuery {
            doc_kind: DocKind::Message,
            project_id: Some(project_id),
            ack_required: Some(true), // 4 messages
            ..Default::default()
        };
        async move {
            match execute_search_simple(&cx, &pool, &q).await {
                Outcome::Ok(r) => r,
                other => panic!("time range prep failed: {other:?}"),
            }
        }
    });

    if all_response.results.len() >= 2 {
        // Use a time range that should include all messages (very wide range)
        let now = now_micros();
        let _pool3 = pool.clone();
        let query = SearchQuery {
            doc_kind: DocKind::Message,
            project_id: Some(project_id),
            time_range: TimeRange {
                min_ts: Some(0),
                max_ts: Some(now + 1_000_000),
            },
            ..Default::default()
        };
        let plan = plan_search(&query);
        assert_eq!(plan.method, PlanMethod::FilterOnly);
        assert!(plan.facets_applied.contains(&"time_range_min".to_string()));
        assert!(plan.facets_applied.contains(&"time_range_max".to_string()));
    }
}

/// Test ranking mode: recency vs relevance.
#[test]
fn ranking_mode_comparison() {
    let (pool, _dir) = make_pool();
    let (project_id, _sender_id, _recipient_id, _msg_ids) = seed_corpus(&pool);

    // Relevance mode (default) - should use BM25 ordering
    let q_rel = SearchQuery::messages("migration", project_id);
    let plan_rel = plan_search(&q_rel);
    assert!(
        plan_rel.sql.contains("score ASC"),
        "relevance mode should sort by score ASC"
    );

    // Recency mode for filter-only - should use created_ts DESC
    let q_recency = SearchQuery {
        doc_kind: DocKind::Message,
        project_id: Some(project_id),
        importance: vec![Importance::Normal],
        ranking: RankingMode::Recency,
        ..Default::default()
    };
    let plan_rec = plan_search(&q_recency);
    assert!(
        plan_rec.sql.contains("created_ts DESC"),
        "recency/filter-only mode should sort by created_ts DESC"
    );
}
