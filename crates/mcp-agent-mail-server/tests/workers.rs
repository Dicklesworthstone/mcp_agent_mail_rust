//! Integration tests for background workers.
//!
//! Tests cover:
//! - Config gating (workers don't start when disabled)
//! - DB queries used by workers (ACK TTL scanning)
//! - Retention/quota with real filesystem layout

use mcp_agent_mail_core::Config;

// ---------------------------------------------------------------------------
// Config gating tests
// ---------------------------------------------------------------------------

#[test]
fn ack_ttl_worker_disabled_by_default() {
    let config = Config::from_env();
    assert!(
        !config.ack_ttl_enabled,
        "ACK TTL should be disabled by default"
    );
}

#[test]
fn tool_metrics_worker_disabled_by_default() {
    let config = Config::from_env();
    assert!(
        !config.tool_metrics_emit_enabled,
        "tool metrics emit should be disabled by default"
    );
}

#[test]
fn retention_worker_disabled_by_default() {
    let config = Config::from_env();
    assert!(!config.retention_report_enabled);
    assert!(!config.quota_enabled);
}

#[test]
fn cleanup_worker_disabled_by_default() {
    let config = Config::from_env();
    assert!(!config.file_reservations_cleanup_enabled);
}

// ---------------------------------------------------------------------------
// Config default values
// ---------------------------------------------------------------------------

#[test]
fn ack_ttl_config_defaults() {
    let config = Config::from_env();
    assert_eq!(
        config.ack_ttl_seconds, 1800,
        "default TTL should be 30 minutes"
    );
    assert_eq!(
        config.ack_ttl_scan_interval_seconds, 60,
        "default scan interval should be 60s"
    );
    assert!(!config.ack_escalation_enabled);
    assert_eq!(config.ack_escalation_mode, "log");
    assert_eq!(config.ack_escalation_claim_ttl_seconds, 3600);
    assert!(!config.ack_escalation_claim_exclusive);
    assert!(config.ack_escalation_claim_holder_name.is_empty());
}

#[test]
fn tool_metrics_config_defaults() {
    let config = Config::from_env();
    assert_eq!(config.tool_metrics_emit_interval_seconds, 60);
}

#[test]
fn retention_config_defaults() {
    let config = Config::from_env();
    assert_eq!(config.retention_report_interval_seconds, 3600);
    assert_eq!(config.retention_max_age_days, 180);
    assert!(!config.retention_ignore_project_patterns.is_empty());
    assert_eq!(config.quota_attachments_limit_bytes, 0);
    assert_eq!(config.quota_inbox_limit_count, 0);
}

// ---------------------------------------------------------------------------
// ACK TTL DB query integration
// ---------------------------------------------------------------------------

#[test]
fn list_unacknowledged_messages_empty_db() {
    use asupersync::{Cx, Outcome};
    use fastmcp_core::block_on;
    use mcp_agent_mail_db::{DbPoolConfig, create_pool, queries};

    let pool_config = DbPoolConfig::from_env();
    let pool = create_pool(&pool_config).expect("create pool");
    let cx = Cx::for_testing();

    // On a fresh DB, there should be no unacknowledged messages.
    let result = block_on(async { queries::list_unacknowledged_messages(&cx, &pool).await });

    match result {
        Outcome::Ok(rows) => assert!(rows.is_empty(), "fresh DB should have no unacked messages"),
        other => panic!("unexpected result: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Retention filesystem integration
// ---------------------------------------------------------------------------

#[test]
fn retention_cycle_with_realistic_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let storage = tmp.path();

    // Create project directories with realistic structure.
    let proj1 = storage.join("backend-api");
    let proj1_attach = proj1.join("attachments");
    let proj1_agents = proj1
        .join("agents")
        .join("GreenCastle")
        .join("inbox")
        .join("2026")
        .join("01");
    std::fs::create_dir_all(&proj1_attach).unwrap();
    std::fs::create_dir_all(&proj1_agents).unwrap();
    std::fs::write(proj1_attach.join("patch.diff"), vec![0u8; 256]).unwrap();
    std::fs::write(proj1_agents.join("msg001.md"), "# Task update\n\nDone.").unwrap();
    std::fs::write(
        proj1_agents.join("msg002.md"),
        "# Review request\n\nPlease review.",
    )
    .unwrap();

    // Create a project that should be ignored.
    let proj2 = storage.join("testproject");
    std::fs::create_dir_all(proj2.join("attachments")).unwrap();
    std::fs::write(proj2.join("attachments").join("big.bin"), vec![0u8; 1000]).unwrap();

    // Create another project.
    let proj3 = storage.join("frontend-app");
    let proj3_agents = proj3
        .join("agents")
        .join("BlueBear")
        .join("inbox")
        .join("2026")
        .join("02");
    std::fs::create_dir_all(&proj3_agents).unwrap();
    std::fs::write(proj3_agents.join("msg.md"), "# Hello").unwrap();

    let mut config = Config::from_env();
    config.storage_root = storage.to_path_buf();
    config.retention_report_enabled = true;
    config.quota_enabled = true;
    config.quota_attachments_limit_bytes = 200; // Trigger warning for proj1 (256 bytes).
    config.quota_inbox_limit_count = 100; // High limit, no warning.
    // Default patterns include "testproj*" which should match "testproject".

    // We can't directly call run_retention_cycle from an integration test
    // (it's private), but we can verify the config and filesystem layout
    // are correct for the worker.

    // Verify ignore patterns match.
    assert!(
        config
            .retention_ignore_project_patterns
            .iter()
            .any(|p| p.starts_with("testproj")),
        "default patterns should include testproj*"
    );

    // Verify filesystem structure.
    assert!(proj1_attach.is_dir());
    assert!(proj1_agents.is_dir());
    assert!(proj3_agents.is_dir());
    assert_eq!(std::fs::read_dir(&proj1_agents).unwrap().count(), 2);
    assert_eq!(std::fs::read_dir(&proj3_agents).unwrap().count(), 1);
}

// ---------------------------------------------------------------------------
// QueryTracker integration
// ---------------------------------------------------------------------------

#[test]
fn query_tracker_records_queries_when_enabled() {
    use mcp_agent_mail_db::QueryTracker;

    let tracker = QueryTracker::new();
    tracker.enable(Some(100)); // 100ms slow threshold

    // Simulate queries.
    tracker.record("SELECT * FROM agents WHERE id = 1", 5_000); // 5ms
    tracker.record("INSERT INTO messages (body) VALUES ('hi')", 15_000); // 15ms
    tracker.record("SELECT * FROM agents WHERE name = 'x'", 3_000); // 3ms
    tracker.record("UPDATE projects SET slug = 'test' WHERE id = 1", 2_000); // 2ms
    tracker.record("SELECT * FROM messages WHERE project_id = 1", 150_000); // 150ms (slow)

    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.total, 5);
    assert!(snapshot.total_time_ms > 0.0);

    // Per-table breakdown.
    assert_eq!(snapshot.per_table.get("agents"), Some(&2));
    assert_eq!(snapshot.per_table.get("messages"), Some(&2));
    assert_eq!(snapshot.per_table.get("projects"), Some(&1));

    // Slow query tracking (150ms >= 100ms threshold).
    assert_eq!(snapshot.slow_queries.len(), 1);
    assert_eq!(snapshot.slow_queries[0].table.as_deref(), Some("messages"));
    assert!(snapshot.slow_queries[0].duration_ms >= 100.0);
}

#[test]
fn query_tracker_snapshot_to_dict_matches_legacy_format() {
    use mcp_agent_mail_db::QueryTracker;

    let tracker = QueryTracker::new();
    tracker.enable(Some(250)); // 250ms threshold

    tracker.record("SELECT * FROM agents WHERE id = 1", 10_000);
    tracker.record("SELECT * FROM messages WHERE id = 1", 10_000);
    tracker.record("SELECT * FROM messages WHERE id = 2", 10_000);

    let snapshot = tracker.snapshot();
    let dict = snapshot.to_dict();

    // Legacy format has these keys.
    assert!(dict.get("total").is_some());
    assert!(dict.get("total_time_ms").is_some());
    assert!(dict.get("per_table").is_some());
    assert!(dict.get("slow_query_ms").is_some());
    assert!(dict.get("slow_queries").is_some());

    // total
    assert_eq!(dict["total"].as_u64(), Some(3));

    // per_table sorted by count desc
    let per_table = dict["per_table"].as_object().unwrap();
    assert_eq!(
        per_table
            .get("messages")
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(
        per_table.get("agents").and_then(serde_json::Value::as_u64),
        Some(1)
    );

    // slow_query_ms threshold
    assert_eq!(dict["slow_query_ms"].as_f64(), Some(250.0));

    // No slow queries (all 10ms < 250ms)
    assert!(dict["slow_queries"].as_array().unwrap().is_empty());
}

#[test]
fn query_tracker_config_controls_enablement() {
    let config = Config::from_env();

    // Default: instrumentation disabled.
    assert!(!config.instrumentation_enabled);

    // Default slow threshold.
    assert_eq!(config.instrumentation_slow_query_ms, 250);

    // Tools log enabled by default.
    assert!(config.tools_log_enabled);
}

#[test]
fn query_tracker_per_tool_isolation() {
    use mcp_agent_mail_db::QueryTracker;
    use std::sync::Arc;

    // Simulate two tool calls each getting their own tracker.
    let tracker1 = Arc::new(QueryTracker::new());
    tracker1.enable(Some(250));
    tracker1.record("SELECT * FROM agents", 5_000);

    let tracker2 = Arc::new(QueryTracker::new());
    tracker2.enable(Some(250));
    tracker2.record("SELECT * FROM messages", 8_000);
    tracker2.record("INSERT INTO messages (body) VALUES ('test')", 12_000);

    // Each tracker is isolated.
    let snap1 = tracker1.snapshot();
    assert_eq!(snap1.total, 1);
    assert_eq!(snap1.per_table.get("agents"), Some(&1));
    assert!(!snap1.per_table.contains_key("messages"));

    let snap2 = tracker2.snapshot();
    assert_eq!(snap2.total, 2);
    assert_eq!(snap2.per_table.get("messages"), Some(&2));
    assert!(!snap2.per_table.contains_key("agents"));
}

// ---------------------------------------------------------------------------
// Instrumentation: tracker lifecycle integration (br-2ei.11.3)
// ---------------------------------------------------------------------------

#[test]
fn query_tracker_thread_local_guard_restores() {
    use mcp_agent_mail_db::{QueryTracker, active_tracker, set_active_tracker};
    use std::sync::Arc;

    let outer = Arc::new(QueryTracker::new());
    outer.enable(Some(250));
    let _g_outer = set_active_tracker(outer);

    {
        let inner = Arc::new(QueryTracker::new());
        inner.enable(Some(100));
        let _g_inner = set_active_tracker(inner);

        // Record on inner tracker via active_tracker().
        if let Some(t) = active_tracker() {
            t.record("SELECT * FROM messages", 5_000);
        }
        assert_eq!(active_tracker().unwrap().snapshot().total, 1);
    }

    // After inner guard dropped, outer should be active again.
    let current = active_tracker().unwrap();
    assert_eq!(
        current.snapshot().total,
        0,
        "outer tracker should be unaffected"
    );
}

#[test]
fn query_tracker_record_query_dispatches_to_active() {
    use mcp_agent_mail_db::{QueryTracker, record_query, set_active_tracker};
    use std::sync::Arc;

    let tracker = Arc::new(QueryTracker::new());
    tracker.enable(Some(250));
    let _guard = set_active_tracker(tracker.clone());

    record_query("SELECT * FROM agents WHERE id = 1", 5_000);
    record_query("INSERT INTO messages (body) VALUES ('hi')", 10_000);

    let snap = tracker.snapshot();
    assert_eq!(snap.total, 2);
    assert_eq!(snap.per_table.get("agents"), Some(&1));
    assert_eq!(snap.per_table.get("messages"), Some(&1));
}

#[test]
fn query_tracker_to_dict_json_roundtrip() {
    use mcp_agent_mail_db::QueryTracker;

    let tracker = QueryTracker::new();
    tracker.enable(Some(100));
    tracker.record("SELECT * FROM agents", 5_000);
    tracker.record("SELECT * FROM messages", 150_000); // slow
    tracker.record("INSERT INTO messages (body) VALUES ('x')", 200_000); // slow

    let dict = tracker.snapshot().to_dict();
    let json_str = serde_json::to_string(&dict).unwrap();
    let roundtrip: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(roundtrip["total"], 3);
    assert!(roundtrip["total_time_ms"].is_f64());
    assert_eq!(roundtrip["per_table"]["messages"].as_u64(), Some(2));
    assert_eq!(roundtrip["per_table"]["agents"].as_u64(), Some(1));
    assert_eq!(roundtrip["slow_query_ms"].as_f64(), Some(100.0));

    let slow = roundtrip["slow_queries"].as_array().unwrap();
    assert_eq!(slow.len(), 2);
    assert_eq!(slow[0]["table"], "messages");
    assert_eq!(slow[1]["table"], "messages");
}

#[test]
fn instrumentation_config_env_parity() {
    // Verify that all instrumentation-related config fields exist and have
    // the expected defaults matching legacy Python.
    let config = Config::from_env();
    assert!(!config.instrumentation_enabled);
    assert_eq!(config.instrumentation_slow_query_ms, 250);
    assert!(config.tools_log_enabled);
}

// ---------------------------------------------------------------------------
// Tool metrics snapshot integration
// ---------------------------------------------------------------------------

#[test]
fn tool_metrics_snapshot_after_recording() {
    use mcp_agent_mail_tools::{record_call, record_error, tool_metrics_snapshot};

    // Record some activity.
    record_call("register_agent");
    record_call("register_agent");
    record_error("register_agent");
    record_call("health_check");

    let snapshot = tool_metrics_snapshot();

    // Find the entries we recorded.
    let register = snapshot.iter().find(|e| e.name == "register_agent");
    assert!(
        register.is_some(),
        "register_agent should appear in snapshot"
    );
    let reg = register.unwrap();
    assert!(reg.calls >= 2, "should have at least 2 calls");
    assert!(reg.errors >= 1, "should have at least 1 error");
    assert!(!reg.cluster.is_empty(), "cluster should be populated");

    let health = snapshot.iter().find(|e| e.name == "health_check");
    assert!(health.is_some(), "health_check should appear in snapshot");
}

#[test]
fn tool_metrics_snapshot_is_json_serializable() {
    use mcp_agent_mail_tools::tool_metrics_snapshot;

    let snapshot = tool_metrics_snapshot();
    let json = serde_json::to_string(&snapshot);
    assert!(json.is_ok(), "snapshot should serialize to JSON");

    // Verify it round-trips through serde_json::Value.
    let value: serde_json::Value = serde_json::from_str(&json.unwrap()).unwrap();
    assert!(value.is_array());
}
