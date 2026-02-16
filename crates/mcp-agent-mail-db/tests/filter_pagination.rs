//! E2E filter-boundary and pagination-stability matrix.
//!
//! Validates correctness of:
//! - Date/time range boundaries (inclusive endpoints, min-only, max-only, both)
//! - Sender/project/thread filter combinations
//! - Importance vector filtering
//! - Cursor-based pagination stability (determinism, page exhaustion)
//! - Cross-filter pagination (filters + pagination combined)
//!
//! Bead: br-2tnl.7.18

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args,
    clippy::identity_op
)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};

use mcp_agent_mail_db::search_planner::{Importance, RankingMode, SearchQuery, TimeRange};
use mcp_agent_mail_db::search_service::{execute_search_simple, SimpleSearchResponse};
use mcp_agent_mail_db::{queries, DbPool, DbPoolConfig};

// ── Helpers ──────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir.path().join(format!("fp_{}.db", unique_suffix()));
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

/// Create a project and return its ID.
fn seed_project(pool: &DbPool, slug: &str) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let key = format!("/test/{}", slug);
        async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(p) => p.id.expect("project id"),
                other => panic!("ensure_project failed: {other:?}"),
            }
        }
    })
}

/// Register an agent and return its ID.
fn seed_agent(pool: &DbPool, project_id: i64, name: &str) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let name = name.to_string();
        async move {
            match queries::register_agent(
                &cx, &pool, project_id, &name, "test", "test", None, None,
            )
            .await
            {
                Outcome::Ok(a) => a.id.expect("agent id"),
                other => panic!("register_agent failed: {other:?}"),
            }
        }
    })
}

/// Create a message with specific importance and thread. Returns message ID.
#[allow(clippy::too_many_arguments)]
fn create_msg(
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body: &str,
    importance: &str,
    thread_id: Option<&str>,
    ack_required: bool,
) -> i64 {
    block_on(|cx| {
        let pool = pool.clone();
        let subject = subject.to_string();
        let body = body.to_string();
        let importance = importance.to_string();
        let thread_id = thread_id.map(String::from);
        async move {
            match queries::create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                &subject,
                &body,
                thread_id.as_deref(),
                &importance,
                ack_required,
                "[]",
            )
            .await
            {
                Outcome::Ok(row) => row.id.expect("message id"),
                other => panic!("create_message failed: {other:?}"),
            }
        }
    })
}

/// Update a message's `created_ts` via raw SQL (for date boundary tests).
fn set_message_ts(pool: &DbPool, msg_id: i64, ts: i64) {
    block_on(|cx| {
        let pool = pool.clone();
        async move {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            conn.execute_raw(&format!(
                "UPDATE messages SET created_ts = {} WHERE id = {}",
                ts, msg_id
            ))
            .expect("update created_ts");
        }
    });
}

/// Execute a search query, returning the response.
fn search(pool: &DbPool, query: &SearchQuery) -> SimpleSearchResponse {
    block_on(|cx| {
        let pool = pool.clone();
        let query = query.clone();
        async move {
            match execute_search_simple(&cx, &pool, &query).await {
                Outcome::Ok(resp) => resp,
                other => panic!("search failed: {other:?}"),
            }
        }
    })
}

/// Collect all result IDs from a search response.
fn result_ids(resp: &SimpleSearchResponse) -> Vec<i64> {
    resp.results.iter().map(|r| r.id).collect()
}

// ── Microsecond timestamp helpers ────────────────────────────────────

const MICROS_PER_SEC: i64 = 1_000_000;
const MICROS_PER_HOUR: i64 = 3_600 * MICROS_PER_SEC;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SEC;

/// 2026-01-15 00:00:00 UTC in microseconds (a fixed reference point).
const BASE_TS: i64 = 1_768_435_200 * MICROS_PER_SEC;

// ── Tests ────────────────────────────────────────────────────────────

/// Test: `min_ts`-only filter returns messages at and after the boundary.
#[test]
fn date_range_min_ts_only() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "dr-min");
    let aid = seed_agent(&pool, pid, "RedFox");

    // Create 5 messages at different timestamps
    let ids: Vec<i64> = (0..5)
        .map(|i| {
            let id = create_msg(
                &pool,
                pid,
                aid,
                &format!("daterange msg{}", i),
                &format!("daterange body{}", i),
                "normal",
                None,
                false,
            );
            set_message_ts(&pool, id, BASE_TS + i * MICROS_PER_HOUR);
            id
        })
        .collect();

    // min_ts = BASE_TS + 2h → should return messages 2, 3, 4
    let mut q = SearchQuery::messages("daterange", pid);
    q.time_range = TimeRange {
        min_ts: Some(BASE_TS + 2 * MICROS_PER_HOUR),
        max_ts: None,
    };

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert!(found.contains(&ids[2]), "msg2 at boundary should be included");
    assert!(found.contains(&ids[3]), "msg3 after boundary should be included");
    assert!(found.contains(&ids[4]), "msg4 after boundary should be included");
    assert!(!found.contains(&ids[0]), "msg0 before boundary excluded");
    assert!(!found.contains(&ids[1]), "msg1 before boundary excluded");
}

/// Test: `max_ts`-only filter returns messages at and before the boundary.
#[test]
fn date_range_max_ts_only() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "dr-max");
    let aid = seed_agent(&pool, pid, "BlueLake");

    let ids: Vec<i64> = (0..5)
        .map(|i| {
            let id = create_msg(
                &pool,
                pid,
                aid,
                &format!("maxrange msg{}", i),
                &format!("maxrange body{}", i),
                "normal",
                None,
                false,
            );
            set_message_ts(&pool, id, BASE_TS + i * MICROS_PER_HOUR);
            id
        })
        .collect();

    // max_ts = BASE_TS + 2h → should return messages 0, 1, 2
    let mut q = SearchQuery::messages("maxrange", pid);
    q.time_range = TimeRange {
        min_ts: None,
        max_ts: Some(BASE_TS + 2 * MICROS_PER_HOUR),
    };

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert!(found.contains(&ids[0]), "msg0 before boundary included");
    assert!(found.contains(&ids[1]), "msg1 before boundary included");
    assert!(found.contains(&ids[2]), "msg2 at boundary included (inclusive)");
    assert!(!found.contains(&ids[3]), "msg3 after boundary excluded");
    assert!(!found.contains(&ids[4]), "msg4 after boundary excluded");
}

/// Test: both `min_ts` and `max_ts` form a closed range.
#[test]
fn date_range_closed_interval() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "dr-closed");
    let aid = seed_agent(&pool, pid, "GreenHawk");

    let ids: Vec<i64> = (0..7)
        .map(|i| {
            let id = create_msg(
                &pool,
                pid,
                aid,
                &format!("closed msg{}", i),
                &format!("closed body{}", i),
                "normal",
                None,
                false,
            );
            set_message_ts(&pool, id, BASE_TS + i * MICROS_PER_DAY);
            id
        })
        .collect();

    // Range: day 2 through day 4 (inclusive)
    let mut q = SearchQuery::messages("closed", pid);
    q.time_range = TimeRange {
        min_ts: Some(BASE_TS + 2 * MICROS_PER_DAY),
        max_ts: Some(BASE_TS + 4 * MICROS_PER_DAY),
    };

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert_eq!(found.len(), 3, "exactly 3 messages in closed range");
    assert!(found.contains(&ids[2]), "day 2 at min boundary");
    assert!(found.contains(&ids[3]), "day 3 inside range");
    assert!(found.contains(&ids[4]), "day 4 at max boundary");
}

/// Test: empty date range (min > max) returns no results.
#[test]
fn date_range_empty_when_inverted() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "dr-empty");
    let aid = seed_agent(&pool, pid, "GoldWolf");

    create_msg(
        &pool, pid, aid,
        "empty-range msg",
        "empty-range body",
        "normal", None, false,
    );

    let mut q = SearchQuery::messages("empty-range", pid);
    q.time_range = TimeRange {
        min_ts: Some(BASE_TS + 10 * MICROS_PER_DAY),
        max_ts: Some(BASE_TS), // max < min
    };

    let resp = search(&pool, &q);
    assert!(resp.results.is_empty(), "inverted range should return 0 results");
}

/// Test: importance vector filter with multiple values.
#[test]
fn importance_filter_multi() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "imp-multi");
    let aid = seed_agent(&pool, pid, "SilverPeak");

    let id_low = create_msg(&pool, pid, aid, "imp low msg", "imp low", "low", None, false);
    let id_normal = create_msg(&pool, pid, aid, "imp normal msg", "imp normal", "normal", None, false);
    let id_high = create_msg(&pool, pid, aid, "imp high msg", "imp high", "high", None, false);
    let id_urgent = create_msg(&pool, pid, aid, "imp urgent msg", "imp urgent", "urgent", None, false);

    // Filter: high + urgent only
    let mut q = SearchQuery::messages("imp", pid);
    q.importance = vec![Importance::High, Importance::Urgent];

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert!(found.contains(&id_high), "high included");
    assert!(found.contains(&id_urgent), "urgent included");
    assert!(!found.contains(&id_low), "low excluded");
    assert!(!found.contains(&id_normal), "normal excluded");
}

/// Test: single importance filter.
#[test]
fn importance_filter_single() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "imp-single");
    let aid = seed_agent(&pool, pid, "DarkElm");

    create_msg(&pool, pid, aid, "isf low msg", "isf low", "low", None, false);
    let id_urgent = create_msg(&pool, pid, aid, "isf urgent msg", "isf urgent", "urgent", None, false);
    create_msg(&pool, pid, aid, "isf normal msg", "isf normal", "normal", None, false);

    let mut q = SearchQuery::messages("isf", pid);
    q.importance = vec![Importance::Urgent];

    let resp = search(&pool, &q);
    assert_eq!(resp.results.len(), 1, "only urgent message");
    assert_eq!(resp.results[0].id, id_urgent);
}

/// Test: `thread_id` filter returns only messages in that thread.
#[test]
fn thread_id_filter() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "thd-filter");
    let aid = seed_agent(&pool, pid, "CalmPine");

    let id_t1 = create_msg(
        &pool, pid, aid,
        "thfilter alpha msg",
        "thfilter alpha body",
        "normal", Some("thread-alpha"), false,
    );
    let _id_t2 = create_msg(
        &pool, pid, aid,
        "thfilter beta msg",
        "thfilter beta body",
        "normal", Some("thread-beta"), false,
    );
    let id_t1b = create_msg(
        &pool, pid, aid,
        "thfilter alpha reply",
        "thfilter alpha reply body",
        "normal", Some("thread-alpha"), false,
    );

    let mut q = SearchQuery::messages("thfilter", pid);
    q.thread_id = Some("thread-alpha".to_string());

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert_eq!(found.len(), 2, "two messages in thread-alpha");
    assert!(found.contains(&id_t1));
    assert!(found.contains(&id_t1b));
}

/// Test: `ack_required` filter.
#[test]
fn ack_required_filter() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "ack-filt");
    let aid = seed_agent(&pool, pid, "SwiftDeer");

    let id_ack = create_msg(
        &pool, pid, aid,
        "ackfilt needs ack",
        "ackfilt ack body",
        "normal", None, true,
    );
    let _id_no = create_msg(
        &pool, pid, aid,
        "ackfilt no ack",
        "ackfilt no ack body",
        "normal", None, false,
    );

    let mut q = SearchQuery::messages("ackfilt", pid);
    q.ack_required = Some(true);

    let resp = search(&pool, &q);
    assert_eq!(resp.results.len(), 1);
    assert_eq!(resp.results[0].id, id_ack);
}

/// Test: project isolation — messages in other projects are not returned.
#[test]
fn project_isolation() {
    let (pool, _dir) = make_pool();
    let pid1 = seed_project(&pool, "proj-iso-a");
    let pid2 = seed_project(&pool, "proj-iso-b");
    let aid1 = seed_agent(&pool, pid1, "MistyFox");
    let aid2 = seed_agent(&pool, pid2, "FoggyWolf");

    let id_p1 = create_msg(&pool, pid1, aid1, "projiso shared word", "projiso body a", "normal", None, false);
    let _id_p2 = create_msg(&pool, pid2, aid2, "projiso shared word", "projiso body b", "normal", None, false);

    let q = SearchQuery::messages("projiso", pid1);
    let resp = search(&pool, &q);
    assert_eq!(resp.results.len(), 1, "only project 1 message");
    assert_eq!(resp.results[0].id, id_p1);
}

/// Test: cursor-based pagination returns all results across pages without duplicates.
#[test]
fn pagination_exhaustive_no_duplicates() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "pag-exhaust");
    let aid = seed_agent(&pool, pid, "BrightOwl");

    for i in 0..10 {
        create_msg(
            &pool, pid, aid,
            &format!("pagexhaust item{}", i),
            &format!("pagexhaust body{}", i),
            "normal", None, false,
        );
    }

    // Page through with limit=3
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;

    loop {
        let mut q = SearchQuery::messages("pagexhaust", pid);
        q.limit = Some(3);
        q.cursor = cursor.clone();

        let resp = search(&pool, &q);
        if resp.results.is_empty() {
            break;
        }

        for r in &resp.results {
            collected.push(r.id);
        }

        pages += 1;
        cursor = resp.next_cursor.clone();
        if cursor.is_none() {
            break;
        }

        // Safety: prevent infinite loops
        assert!(pages <= 10, "too many pages");
    }

    // All 10 messages should be collected with no duplicates
    let unique: HashSet<i64> = collected.iter().copied().collect();
    assert_eq!(unique.len(), 10, "all 10 unique messages collected");
    assert_eq!(collected.len(), 10, "no duplicates across pages");
}

/// Test: pagination is deterministic across repeated identical queries.
#[test]
fn pagination_determinism() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "pag-det");
    let aid = seed_agent(&pool, pid, "SilverLark");

    for i in 0..8 {
        create_msg(
            &pool, pid, aid,
            &format!("pagdet msg{}", i),
            &format!("pagdet body{}", i),
            "normal", None, false,
        );
    }

    // Run the same paginated query 3 times
    let mut all_runs: Vec<Vec<i64>> = Vec::new();
    for _ in 0..3 {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q = SearchQuery::messages("pagdet", pid);
            q.limit = Some(3);
            q.cursor = cursor.clone();

            let resp = search(&pool, &q);
            if resp.results.is_empty() {
                break;
            }
            for r in &resp.results {
                collected.push(r.id);
            }
            cursor = resp.next_cursor.clone();
            if cursor.is_none() {
                break;
            }
        }
        all_runs.push(collected);
    }

    // All three runs should produce identical ordering
    assert_eq!(all_runs[0], all_runs[1], "run 1 == run 2");
    assert_eq!(all_runs[1], all_runs[2], "run 2 == run 3");
    assert_eq!(all_runs[0].len(), 8, "all 8 messages collected");
}

/// Test: last page has no `next_cursor`.
#[test]
fn pagination_last_page_no_cursor() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "pag-last");
    let aid = seed_agent(&pool, pid, "FrostyRaven");

    for i in 0..5 {
        create_msg(
            &pool, pid, aid,
            &format!("paglast msg{}", i),
            &format!("paglast body{}", i),
            "normal", None, false,
        );
    }

    // limit=5 should return all in one page with no cursor
    let mut q = SearchQuery::messages("paglast", pid);
    q.limit = Some(5);

    let resp = search(&pool, &q);
    assert_eq!(resp.results.len(), 5);
    // When all results fit in one page, no cursor needed
    // (cursor may or may not be None depending on whether the query checks for limit+1)

    // limit=10 (more than exist) definitely no cursor
    let mut q2 = SearchQuery::messages("paglast", pid);
    q2.limit = Some(10);

    let resp2 = search(&pool, &q2);
    assert_eq!(resp2.results.len(), 5);
    assert!(resp2.next_cursor.is_none(), "no cursor when all results returned");
}

/// Test: recency ranking with `FilterOnly` mode returns newest-first.
///
/// Note: `RankingMode::Recency` only affects FilterOnly/LIKE queries, not FTS.
/// FTS always ranks by BM25 score. This test uses a filter-only query
/// (empty text, project + importance filter) to validate recency ordering.
#[test]
fn recency_ranking_order() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "rank-rec");
    let aid = seed_agent(&pool, pid, "ProudCedar");

    for i in 0..5 {
        let id = create_msg(
            &pool, pid, aid,
            &format!("recency msg{}", i),
            &format!("recency body{}", i),
            "high", None, false,
        );
        // Set timestamps with known ordering: id[0]=oldest, id[4]=newest
        set_message_ts(&pool, id, BASE_TS + i * MICROS_PER_HOUR);
    }

    // Filter-only query: no text, just importance filter + project scoping
    let mut q = SearchQuery {
        project_id: Some(pid),
        importance: vec![Importance::High],
        ranking: RankingMode::Recency,
        ..Default::default()
    };
    q.limit = Some(10);

    let resp = search(&pool, &q);
    assert!(resp.results.len() >= 5, "at least 5 results");

    // Recency = newest first, so timestamps should decrease
    for w in resp.results.windows(2) {
        let ts_a = w[0].created_ts.unwrap_or(0);
        let ts_b = w[1].created_ts.unwrap_or(0);
        assert!(ts_a >= ts_b, "recency order: ts {} >= ts {}", ts_a, ts_b);
    }
}

/// Test: combined filters — date range + importance + thread.
#[test]
fn combined_filters() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "combined");
    let aid = seed_agent(&pool, pid, "QuietHawk");

    // Create messages with varying attributes
    let id1 = create_msg(
        &pool, pid, aid,
        "combofilter alpha",
        "combofilter alpha body",
        "high", Some("thread-x"), false,
    );
    set_message_ts(&pool, id1, BASE_TS + 1 * MICROS_PER_HOUR);

    let id2 = create_msg(
        &pool, pid, aid,
        "combofilter beta",
        "combofilter beta body",
        "normal", Some("thread-x"), false,
    );
    set_message_ts(&pool, id2, BASE_TS + 2 * MICROS_PER_HOUR);

    let id3 = create_msg(
        &pool, pid, aid,
        "combofilter gamma",
        "combofilter gamma body",
        "high", Some("thread-y"), false,
    );
    set_message_ts(&pool, id3, BASE_TS + 3 * MICROS_PER_HOUR);

    let id4 = create_msg(
        &pool, pid, aid,
        "combofilter delta",
        "combofilter delta body",
        "high", Some("thread-x"), false,
    );
    set_message_ts(&pool, id4, BASE_TS + 5 * MICROS_PER_HOUR);

    // Filter: high importance + thread-x + date range [+1h, +4h]
    let mut q = SearchQuery::messages("combofilter", pid);
    q.importance = vec![Importance::High];
    q.thread_id = Some("thread-x".to_string());
    q.time_range = TimeRange {
        min_ts: Some(BASE_TS + 1 * MICROS_PER_HOUR),
        max_ts: Some(BASE_TS + 4 * MICROS_PER_HOUR),
    };

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    // id1: high + thread-x + 1h (in range) → included
    // id2: normal + thread-x → excluded (importance)
    // id3: high + thread-y → excluded (thread)
    // id4: high + thread-x + 5h → excluded (date)
    assert_eq!(found.len(), 1, "only id1 matches all filters");
    assert!(found.contains(&id1));
}

/// Test: pagination with filters applied — filtered set is paginated correctly.
#[test]
fn pagination_with_filters() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "pag-filt");
    let aid = seed_agent(&pool, pid, "NobleLion");

    // Create 12 messages: 6 high importance, 6 normal
    let mut high_ids = Vec::new();
    for i in 0..12 {
        let imp = if i % 2 == 0 { "high" } else { "normal" };
        let id = create_msg(
            &pool, pid, aid,
            &format!("pagfilt item{}", i),
            &format!("pagfilt body{}", i),
            imp, None, false,
        );
        if i % 2 == 0 {
            high_ids.push(id);
        }
    }

    // Paginate through high-importance only, limit=2
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;

    loop {
        let mut q = SearchQuery::messages("pagfilt", pid);
        q.importance = vec![Importance::High];
        q.limit = Some(2);
        q.cursor = cursor.clone();

        let resp = search(&pool, &q);
        if resp.results.is_empty() {
            break;
        }

        for r in &resp.results {
            collected.push(r.id);
        }

        pages += 1;
        cursor = resp.next_cursor.clone();
        if cursor.is_none() {
            break;
        }
        assert!(pages <= 10, "too many pages");
    }

    // Should collect exactly the 6 high-importance messages
    let unique: HashSet<i64> = collected.iter().copied().collect();
    assert_eq!(unique.len(), 6, "all 6 high-importance messages");
    for hid in &high_ids {
        assert!(unique.contains(hid), "high-importance msg {} collected", hid);
    }
}

/// Test: date range at exact microsecond boundaries — tests inclusive endpoints.
#[test]
fn date_boundary_exact_microsecond() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "us-boundary");
    let aid = seed_agent(&pool, pid, "CoralOwl");

    let exact_ts = BASE_TS + 42 * MICROS_PER_SEC;

    let id_at = create_msg(
        &pool, pid, aid,
        "usbnd exact msg",
        "usbnd exact body",
        "normal", None, false,
    );
    set_message_ts(&pool, id_at, exact_ts);

    let id_before = create_msg(
        &pool, pid, aid,
        "usbnd before msg",
        "usbnd before body",
        "normal", None, false,
    );
    set_message_ts(&pool, id_before, exact_ts - 1);

    let id_after = create_msg(
        &pool, pid, aid,
        "usbnd after msg",
        "usbnd after body",
        "normal", None, false,
    );
    set_message_ts(&pool, id_after, exact_ts + 1);

    // min_ts = exact → should include exact and after, exclude before
    let mut q = SearchQuery::messages("usbnd", pid);
    q.time_range = TimeRange {
        min_ts: Some(exact_ts),
        max_ts: None,
    };

    let resp = search(&pool, &q);
    let found: HashSet<i64> = result_ids(&resp).into_iter().collect();
    assert!(found.contains(&id_at), "exact microsecond included");
    assert!(found.contains(&id_after), "after microsecond included");
    assert!(!found.contains(&id_before), "before microsecond excluded");

    // max_ts = exact → should include exact and before, exclude after
    let mut q2 = SearchQuery::messages("usbnd", pid);
    q2.time_range = TimeRange {
        min_ts: None,
        max_ts: Some(exact_ts),
    };

    let resp2 = search(&pool, &q2);
    let found2: HashSet<i64> = result_ids(&resp2).into_iter().collect();
    assert!(found2.contains(&id_at), "exact at max boundary included");
    assert!(found2.contains(&id_before), "before max boundary included");
    assert!(!found2.contains(&id_after), "after max boundary excluded");
}

/// Test: FTS pagination with date range — all matching results paginated correctly.
///
/// Uses FTS mode (text query) with date range to test that cursor-based pagination
/// combined with date filters returns the correct subset and paginates fully.
#[test]
fn fts_pagination_with_date_filter() {
    let (pool, _dir) = make_pool();
    let pid = seed_project(&pool, "fts-pag-date");
    let aid = seed_agent(&pool, pid, "CalmHeron");

    // Create 8 messages spanning 8 days
    for i in 0..8 {
        let id = create_msg(
            &pool, pid, aid,
            &format!("ftspagdate msg{}", i),
            &format!("ftspagdate body{}", i),
            "normal", None, false,
        );
        set_message_ts(&pool, id, BASE_TS + i * MICROS_PER_DAY);
    }

    // FTS query with date range days 2-6 (5 messages), paginate with limit=2
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;

    loop {
        let mut q = SearchQuery::messages("ftspagdate", pid);
        q.limit = Some(2);
        q.cursor = cursor.clone();
        q.time_range = TimeRange {
            min_ts: Some(BASE_TS + 2 * MICROS_PER_DAY),
            max_ts: Some(BASE_TS + 6 * MICROS_PER_DAY),
        };

        let resp = search(&pool, &q);
        if resp.results.is_empty() {
            break;
        }

        for r in &resp.results {
            collected.push(r.id);
        }

        pages += 1;
        cursor = resp.next_cursor.clone();
        if cursor.is_none() {
            break;
        }
        assert!(pages <= 10, "too many pages");
    }

    // Should have 5 messages (days 2,3,4,5,6)
    let unique: HashSet<i64> = collected.iter().copied().collect();
    assert_eq!(unique.len(), 5, "5 unique messages in date range");
    assert_eq!(collected.len(), 5, "no duplicates");
}
