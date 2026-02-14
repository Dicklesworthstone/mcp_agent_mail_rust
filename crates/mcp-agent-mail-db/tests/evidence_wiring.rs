//! Integration tests verifying evidence ledger wiring at decision points.
//!
//! Required tests for br-kqelk (B.2) and br-3hkkd (B.3).

use std::time::Instant;

use mcp_agent_mail_core::evidence_ledger::{evidence_ledger, EvidenceLedger, EvidenceLedgerEntry};
use mcp_agent_mail_db::coalesce::CoalesceMap;
use mcp_agent_mail_db::models::ProjectRow;
use mcp_agent_mail_db::read_cache;

fn make_project(slug: &str) -> ProjectRow {
    ProjectRow {
        id: Some(1),
        slug: slug.to_string(),
        human_key: format!("/data/{slug}"),
        created_at: 0,
    }
}

/// 1. Trigger deferred flush via global cache, verify ledger has entry.
#[test]
fn evidence_cache_eviction_recorded() {
    let ledger = evidence_ledger();
    let before = ledger.query("cache.deferred_flush", 1000).len();

    let cache = read_cache();
    cache.enqueue_touch(9901, 1000);
    let _ = cache.drain_touches();

    let after = ledger.query("cache.deferred_flush", 1000).len();
    assert!(
        after > before,
        "expected evidence entry for cache.deferred_flush after drain, before={before} after={after}"
    );
}

/// 2. Run coalesce with concurrent operations, verify entries recorded.
#[test]
fn evidence_coalesce_outcome_recorded() {
    let ledger = evidence_ledger();
    let before = ledger.query("coalesce.outcome", 1000).len();

    let map: CoalesceMap<String, String> =
        CoalesceMap::new(100, std::time::Duration::from_millis(100));
    let result: Result<_, String> =
        map.execute_or_join("test-key".into(), || Ok("hello".to_string()));
    assert!(result.is_ok());

    let after = ledger.query("coalesce.outcome", 1000).len();
    assert!(
        after > before,
        "expected evidence entry for coalesce.outcome, before={before} after={after}"
    );

    let entries = ledger.query("coalesce.outcome", 1);
    assert_eq!(entries[0].decision_point, "coalesce.outcome");
}

/// 3. Trigger deferred flush with multiple touches, verify entry recorded.
#[test]
fn evidence_deferred_flush_recorded() {
    let ledger = evidence_ledger();
    let before = ledger.query("cache.deferred_flush", 1000).len();

    let cache = read_cache();
    cache.enqueue_touch(9801, 1000);
    cache.enqueue_touch(9802, 2000);
    cache.enqueue_touch(9803, 3000);
    let _ = cache.drain_touches();

    let after = ledger.query("cache.deferred_flush", 1000).len();
    assert!(
        after > before,
        "expected evidence entry for cache.deferred_flush, before={before} after={after}"
    );

    let entries = ledger.query("cache.deferred_flush", 1);
    let evidence = &entries[0].evidence;
    // pending_count may be any value (other tests may contribute)
    assert!(evidence.get("pending_count").is_some());
}

/// 4. No regression: 10K inserts, wall time is competitive.
#[test]
fn evidence_no_regression_cache_stress() {
    let n = 10_000;

    // Measure insert throughput — evidence recording is only on
    // eviction/flush, not on every insert, so overhead is minimal.
    let start = Instant::now();
    let cache = read_cache();
    for i in 0..n {
        let p = make_project(&format!("stress-{i}"));
        cache.put_project(&p);
    }
    let elapsed = start.elapsed();

    // 10K inserts should complete well under 5 seconds even on slow CI
    assert!(
        elapsed.as_secs() < 5,
        "10K project inserts took {elapsed:?}, expected < 5s"
    );
}

/// 5. Exercise multiple decision points, verify query returns distinct types.
#[test]
fn evidence_multiple_decision_points() {
    let ledger = evidence_ledger();

    // Trigger cache.deferred_flush
    let cache = read_cache();
    cache.enqueue_touch(9701, 5000);
    let _ = cache.drain_touches();

    // Trigger coalesce.outcome
    let map: CoalesceMap<String, i32> = CoalesceMap::new(50, std::time::Duration::from_millis(50));
    let _: Result<_, String> = map.execute_or_join("multi-test".into(), || Ok(42));

    // Verify distinct decision points are recorded
    let flush_entries = ledger.query("cache.deferred_flush", 100);
    let coalesce_entries = ledger.query("coalesce.outcome", 100);

    assert!(
        !flush_entries.is_empty(),
        "missing cache.deferred_flush entries"
    );
    assert!(
        !coalesce_entries.is_empty(),
        "missing coalesce.outcome entries"
    );

    // Verify they are actually different decision points
    assert_ne!(
        flush_entries[0].decision_point,
        coalesce_entries[0].decision_point
    );
}

// ── B.3 integration tests (br-3hkkd) ─────────────────────────────────────

/// 6. Write entries to JSONL file via EvidenceLedger, read back, verify roundtrip.
#[test]
fn evidence_integration_jsonl_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence_roundtrip.jsonl");

    let ledger = EvidenceLedger::with_file(&path, 500).expect("create file ledger");

    // Record 3 entries with different decision points
    let seq1 = ledger.record(
        "cache.eviction",
        serde_json::json!({"size": 1024, "age_secs": 300}),
        "evict",
        Some("hit_rate >= 0.85".into()),
        0.92,
        "s3fifo_v1",
    );
    let seq2 = ledger.record(
        "coalesce.outcome",
        serde_json::json!({"key": "test", "inflight": 3}),
        "joined",
        Some("join_rate >= 0.3".into()),
        0.80,
        "coalesce_v1",
    );
    let seq3 = ledger.record(
        "tui.diff_strategy",
        serde_json::json!({"frame_budget_ms": 16, "dirty_cells": 42}),
        "incremental",
        Some("frame_time < 16ms".into()),
        0.95,
        "bayesian_tui_v1",
    );

    // Backfill outcomes
    ledger.record_outcome(seq1, "hit_rate=0.87", true);
    ledger.record_outcome(seq2, "join_rate=0.45", true);
    ledger.record_outcome(seq3, "frame_time=22ms", false);

    // Read back JSONL and verify
    let content = std::fs::read_to_string(&path).expect("read JSONL");
    let lines: Vec<&str> = content.lines().collect();

    // 3 record lines + 3 outcome lines = 6 total
    assert_eq!(
        lines.len(),
        6,
        "expected 6 JSONL lines (3 records + 3 outcomes), got {}",
        lines.len()
    );

    // Verify first record
    let record1: EvidenceLedgerEntry = serde_json::from_str(lines[0]).expect("parse record 1");
    assert_eq!(record1.decision_point, "cache.eviction");
    assert_eq!(record1.action, "evict");
    assert_eq!(record1.seq, seq1);
    assert!((record1.confidence - 0.92).abs() < 1e-9);
    assert_eq!(record1.model, "s3fifo_v1");

    // Verify second record
    let record2: EvidenceLedgerEntry = serde_json::from_str(lines[1]).expect("parse record 2");
    assert_eq!(record2.decision_point, "coalesce.outcome");
    assert_eq!(record2.seq, seq2);

    // Verify third record
    let record3: EvidenceLedgerEntry = serde_json::from_str(lines[2]).expect("parse record 3");
    assert_eq!(record3.decision_point, "tui.diff_strategy");
    assert_eq!(record3.seq, seq3);

    // Verify outcome lines
    let outcome1: serde_json::Value = serde_json::from_str(lines[3]).expect("parse outcome 1");
    assert_eq!(outcome1["type"], "outcome");
    assert_eq!(outcome1["seq"], seq1);
    assert_eq!(outcome1["correct"], true);
    assert_eq!(outcome1["actual"], "hit_rate=0.87");

    let outcome3: serde_json::Value = serde_json::from_str(lines[5]).expect("parse outcome 3");
    assert_eq!(outcome3["correct"], false);
    assert_eq!(outcome3["actual"], "frame_time=22ms");

    // Verify in-memory state matches
    let recent = ledger.recent(3);
    assert_eq!(recent.len(), 3);
    // newest-first
    assert_eq!(recent[0].decision_point, "tui.diff_strategy");
    assert_eq!(recent[0].actual.as_deref(), Some("frame_time=22ms"));
    assert_eq!(recent[0].correct, Some(false));

    // Verify hit_rate computation
    let cache_rate = ledger.hit_rate("cache.eviction", 10);
    assert!((cache_rate - 1.0).abs() < 1e-9, "cache hit_rate should be 1.0 (1/1 correct)");

    let tui_rate = ledger.hit_rate("tui.diff_strategy", 10);
    assert!((tui_rate - 0.0).abs() < 1e-9, "tui hit_rate should be 0.0 (0/1 correct)");
}

/// 7. Full storage flow exercises all decision points and verifies entries.
#[test]
fn evidence_integration_all_decision_points() {
    let ledger = evidence_ledger();

    // Exercise cache decision point
    let cache = read_cache();
    cache.enqueue_touch(9601, 6000);
    let _ = cache.drain_touches();

    // Exercise coalesce decision point
    let map: CoalesceMap<String, String> =
        CoalesceMap::new(50, std::time::Duration::from_millis(50));
    let _: Result<_, String> = map.execute_or_join("integration-all".into(), || Ok("ok".into()));

    // Verify at least 2 distinct decision points are recorded
    let all_recent = ledger.recent(100);
    let distinct_dps: std::collections::HashSet<&str> = all_recent
        .iter()
        .map(|e| e.decision_point.as_str())
        .collect();

    assert!(
        distinct_dps.len() >= 2,
        "expected at least 2 distinct decision points, got {}: {:?}",
        distinct_dps.len(),
        distinct_dps
    );
    assert!(
        distinct_dps.contains("cache.deferred_flush"),
        "missing cache.deferred_flush"
    );
    assert!(
        distinct_dps.contains("coalesce.outcome"),
        "missing coalesce.outcome"
    );

    // Verify hit_rate returns valid value in [0.0, 1.0]
    let rate = ledger.hit_rate("cache.deferred_flush", 100);
    assert!(
        (0.0..=1.0).contains(&rate),
        "hit_rate should be in [0.0, 1.0], got {rate}"
    );
}
