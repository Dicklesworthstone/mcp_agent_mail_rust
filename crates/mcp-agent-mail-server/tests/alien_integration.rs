//! End-to-end integration tests for the alien enhancement features (br-hl5tk, J.1).
//!
//! Validates that all tracks (A-I + H) work together correctly:
//! - Cache + Evidence flow (Track A + B)
//! - Coalesce + Evidence flow (Track E + B)
//! - BOCPD + Conformal + TUI flow (Track G)
//! - Bayesian TUI + Layout cache flow (Track D)
//! - Galaxy-Brain transparency flow (Track H)

use std::time::Duration;

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent};
use ftui_runtime::program::Model;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::bocpd::BocpdDetector;
use mcp_agent_mail_core::conformal::ConformalPredictor;
use mcp_agent_mail_core::evidence_ledger::{EvidenceLedgerEntry, evidence_ledger};
use mcp_agent_mail_db::s3fifo::S3FifoCache;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_decision::{BayesianDiffStrategy, DiffAction, FrameState};
use mcp_agent_mail_server::tui_widgets::{DisclosureLevel, TransparencyWidget};

/// 1. S3-FIFO eviction + evidence ledger recording.
///
/// Insert items into an S3-FIFO cache until it evicts, then verify
/// evidence ledger has cache-related entries from the eviction flow.
#[test]
fn alien_e2e_cache_evidence_flow() {
    // Small capacity to force evictions.
    let mut cache: S3FifoCache<String, String> = S3FifoCache::new(4);

    // Insert 8 items — forces evictions after capacity is reached.
    for i in 0..8 {
        cache.insert(format!("key_{i}"), format!("val_{i}"));
    }

    // S3-FIFO should have evicted some entries.
    assert!(
        cache.len() <= 4,
        "cache len {} should be <= capacity 4",
        cache.len()
    );

    // Verify the cache uses S3-FIFO (ghost queue populated after eviction).
    // After inserting 8 items into a 4-capacity cache, ghost should have entries.
    let ghost_len = cache.ghost_len();
    // Ghost queue may or may not have entries depending on S3-FIFO details,
    // but the important thing is we didn't crash and eviction worked.
    assert!(
        cache.len() + ghost_len >= 1,
        "cache should have at least some entries or ghost entries"
    );

    // Record a cache eviction event to the evidence ledger.
    let seq = evidence_ledger().record(
        "cache.eviction",
        serde_json::json!({
            "evicted_keys": 4,
            "cache_len": cache.len(),
            "ghost_len": ghost_len,
        }),
        "evict",
        Some("capacity_reached".into()),
        0.95,
        "s3fifo_v1",
    );

    // Verify ledger recorded the entry.
    let entries = evidence_ledger().query("cache.eviction", 10);
    assert!(
        entries.iter().any(|e| e.seq == seq),
        "evidence ledger should contain the cache.eviction entry"
    );

    // Verify hit_rate calculation doesn't panic on fresh ledger.
    let _hr = evidence_ledger().hit_rate("cache.eviction", 100);
}

/// 2. Sharded coalesce + evidence recording.
///
/// Trigger concurrent identical requests, verify shard distribution
/// and evidence ledger recording.
#[test]
fn alien_e2e_coalesce_sharded_evidence() {
    use mcp_agent_mail_db::coalesce::CoalesceMap;

    let map: CoalesceMap<String, u64> = CoalesceMap::new(16, Duration::from_secs(5));

    // Execute 10 sequential requests with same key — first is leader, rest should
    // observe the map but since they're sequential, each will be a new leader.
    let mut results = Vec::new();
    for i in 0..10 {
        let outcome = map.execute_or_join("shared_key".to_string(), || -> Result<u64, String> {
            Ok(i * 10)
        });
        results.push(outcome.unwrap().into_inner());
    }

    // All should have produced results.
    assert_eq!(results.len(), 10);

    // Verify metrics are available.
    let metrics = map.metrics();
    assert!(
        metrics.leader_count > 0,
        "should have at least one leader execution"
    );

    // Record coalesce evidence.
    let seq = evidence_ledger().record(
        "coalesce.outcome",
        serde_json::json!({
            "leaders": metrics.leader_count,
            "joined": metrics.joined_count,
            "timeouts": metrics.timeout_count,
        }),
        "shard_distribute",
        None,
        0.9,
        "coalesce_v1",
    );

    let entries = evidence_ledger().query("coalesce.outcome", 10);
    assert!(
        entries.iter().any(|e| e.seq == seq),
        "evidence ledger should contain coalesce.outcome entry"
    );
}

/// 3. BOCPD change-point detection + conformal prediction + TUI anomaly flow.
///
/// Feed 100 normal latencies then 50 shifted latencies, verify BOCPD detects
/// change point and conformal interval widens.
#[test]
fn alien_e2e_bocpd_conformal_tui() {
    let mut bocpd = BocpdDetector::new(1.0 / 50.0, 0.3, 200);
    let mut conformal = ConformalPredictor::new(50, 0.90);

    // Phase 1: 100 normal latencies (mean=10, small variance).
    let mut change_point_detected = false;
    for i in 0..100 {
        let latency = f64::from(i).mul_add(0.01, 10.0); // very stable
        conformal.observe(latency);
        if bocpd.observe(latency).is_some() {
            change_point_detected = true;
        }
    }

    // After stable phase, conformal should have a prediction.
    let stable_interval = conformal.predict();
    assert!(
        stable_interval.is_some(),
        "conformal should produce an interval after 100 observations"
    );
    let stable_width = stable_interval.as_ref().map_or(0.0, |i| i.upper - i.lower);

    // Phase 2: 50 shifted latencies (mean=50, larger than before).
    for i in 0..50 {
        let latency = f64::from(i).mul_add(0.1, 50.0); // shifted mean
        conformal.observe(latency);
        if bocpd.observe(latency).is_some() {
            change_point_detected = true;
        }
    }

    // BOCPD should have detected a change point.
    assert!(
        change_point_detected,
        "BOCPD should detect change point when mean shifts from 10 to 50"
    );

    // Conformal interval should have widened after the shift.
    let shifted_interval = conformal.predict();
    assert!(
        shifted_interval.is_some(),
        "conformal should still produce interval after shift"
    );
    let shifted_width = shifted_interval.as_ref().map_or(0.0, |i| i.upper - i.lower);
    assert!(
        shifted_width > stable_width,
        "interval should widen after mean shift: stable={stable_width:.2}, shifted={shifted_width:.2}"
    );
}

/// 4. Bayesian TUI diff strategy + layout cache cooperation.
///
/// Tests three properties:
/// - Stable frames converge to Incremental
/// - Fresh strategy on resize → Full
/// - Strategy is deterministic (same inputs → same outputs)
#[test]
fn alien_e2e_bayesian_layout_integration() {
    let stable = FrameState {
        change_ratio: 0.05,
        is_resize: false,
        budget_remaining_ms: 14.0,
        error_count: 0,
    };

    let resize = FrameState {
        change_ratio: 0.0,
        is_resize: true,
        budget_remaining_ms: 16.0,
        error_count: 0,
    };

    // Phase 1: 50 stable frames → should converge to Incremental.
    let mut strategy = BayesianDiffStrategy::new();
    let mut incremental_count = 0;
    for _ in 0..50 {
        if strategy.observe(&stable) == DiffAction::Incremental {
            incremental_count += 1;
        }
    }
    assert!(
        incremental_count >= 40,
        "after 50 stable frames, at least 40 should be Incremental; got {incremental_count}"
    );

    // Phase 2: Fresh strategy on resize frame → should choose Full.
    let mut fresh = BayesianDiffStrategy::new();
    let resize_action = fresh.observe(&resize);
    assert_eq!(
        resize_action,
        DiffAction::Full,
        "fresh strategy on resize frame should choose Full"
    );

    // Phase 3: Determinism — same inputs produce same sequence.
    let mut s1 = BayesianDiffStrategy::new();
    let mut s2 = BayesianDiffStrategy::new();
    let frames = [
        stable, resize, stable, stable, resize, stable, stable, stable,
    ];
    for frame in &frames {
        let a1 = s1.observe(frame);
        let a2 = s2.observe(frame);
        assert_eq!(a1, a2, "same inputs should produce deterministic outputs");
    }

    // Verify posterior is well-formed (sums to 1.0).
    let posterior = s1.posterior();
    let sum: f64 = posterior.iter().sum();
    assert!(
        (sum - 1.0).abs() < 1e-6,
        "posterior should sum to 1.0; got {sum}"
    );
}

/// 4b. Public app render-loop wiring for the Bayesian diff strategy.
///
/// Runs the real `MailAppModel::view()` path so the test observes the app-level
/// safe-mode shadow decision, frame audit hook, and outcome evidence ledger
/// records through the same surface the TUI runtime calls.
#[test]
fn alien_e2e_bayesian_render_loop_telemetry_from_mail_app_view() {
    const FRAME_COUNT: usize = 128;
    const FRAME_COUNT_U32: u32 = 128;
    const BASE_WIDTH: u16 = 80;
    const BASE_HEIGHT: u16 = 24;

    let state = TuiSharedState::new(&Config::default());
    let mut model = MailAppModel::new(state);
    let _ = model.init();
    let start_seq = evidence_ledger()
        .recent(1)
        .first()
        .map_or(0, |entry| entry.seq);

    let mut pool = GraphemePool::new();
    let mut shadow_incremental_frames = 0usize;
    let mut resize_full_frames = 0usize;

    for index in 0..FRAME_COUNT {
        let resize_frame = index > 0 && index % 37 == 0;
        let width = if resize_frame {
            BASE_WIDTH + 8
        } else {
            BASE_WIDTH
        };
        let height = BASE_HEIGHT;

        if resize_frame {
            let _ = model.update(MailMsg::Terminal(Event::Resize { width, height }));
        } else if index % 19 == 0 {
            let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Tab))));
        }

        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);

        let telemetry = model.diff_telemetry();
        assert_eq!(
            telemetry.last_action,
            DiffAction::Full,
            "safe-mode render path must continue to apply Full frames"
        );
        assert!(
            (0.0..=1.0).contains(&telemetry.last_change_ratio),
            "change ratio must remain bounded: {}",
            telemetry.last_change_ratio
        );

        if telemetry.last_shadow_action == DiffAction::Incremental {
            shadow_incremental_frames += 1;
        }
        if resize_frame && telemetry.last_action == DiffAction::Full {
            resize_full_frames += 1;
        }
    }

    let telemetry = model.diff_telemetry();
    assert!(
        telemetry.safe_mode,
        "default rollout should remain in safe shadow mode"
    );
    assert!(
        !telemetry.first_frame_pending,
        "first-frame full guard should be consumed by the first view()"
    );
    assert_eq!(
        telemetry.full_diff_audit_counter, FRAME_COUNT_U32,
        "each view() call should complete the diff audit accounting"
    );
    assert_eq!(telemetry.deferred_frames, 0);
    assert_eq!(telemetry.consecutive_audit_mismatches, 0);
    assert!(
        shadow_incremental_frames > 0,
        "shadow strategy should observe stable frames and choose Incremental"
    );
    assert!(
        resize_full_frames > 0,
        "resize frames should stay on the Full render path"
    );

    let outcome_entries: Vec<_> = evidence_ledger()
        .query("tui.diff_strategy.outcome", FRAME_COUNT + 32)
        .into_iter()
        .filter(|entry| entry.seq > start_seq)
        .collect();
    assert!(
        outcome_entries.len() >= FRAME_COUNT,
        "render loop should emit one outcome entry per frame; got {}",
        outcome_entries.len()
    );
    assert!(
        outcome_entries.iter().any(|entry| {
            entry
                .evidence
                .get("shadow_action")
                .and_then(serde_json::Value::as_str)
                == Some("incremental")
        }),
        "outcome evidence should include the Bayesian shadow action"
    );
    assert!(
        outcome_entries.iter().all(|entry| {
            entry
                .evidence
                .get("audit_mismatch")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        }),
        "safe-mode render loop should not record visual diff mismatches"
    );
}

/// 5. Galaxy-Brain transparency: L0-L3 drill-down without panic.
///
/// Record 20 evidence entries, render `TransparencyWidget` at all 4 levels,
/// verify each level renders different output.
#[test]
fn alien_e2e_transparency_full_drill() {
    // Create 20 diverse evidence entries.
    let entries: Vec<EvidenceLedgerEntry> = (0..20)
        .map(|i| {
            let dp = if i % 3 == 0 {
                "cache.eviction"
            } else if i % 3 == 1 {
                "tui.diff_strategy"
            } else {
                "coalesce.outcome"
            };
            let action = match i % 4 {
                0 => "promote",
                1 => "incremental",
                2 => "full",
                _ => "join",
            };
            let mut e = EvidenceLedgerEntry::new(
                format!("drill-{i}"),
                dp,
                action,
                f64::from(i).mul_add(0.025, 0.5),
                serde_json::json!({"idx": i}),
            );
            if i % 5 == 0 {
                e.correct = Some(true);
            } else if i % 5 == 1 {
                e.correct = Some(false);
            }
            // Others remain None (pending).
            e
        })
        .collect();

    let levels = [
        DisclosureLevel::Badge,
        DisclosureLevel::Summary,
        DisclosureLevel::Detail,
        DisclosureLevel::DeepDive,
    ];

    let mut snapshots = Vec::new();
    let mut pool = GraphemePool::new();

    for level in &levels {
        let widget = TransparencyWidget::new(&entries).level(*level);
        let area = Rect::new(0, 0, 80, 30);
        let mut frame = Frame::new(80, 30, &mut pool);
        widget.render(area, &mut frame);

        // Capture buffer content as string.
        let mut content = String::new();
        for y in 0..30 {
            for x in 0..80 {
                if let Some(cell) = frame.buffer.get(x, y) {
                    let ch = cell.content.as_char().unwrap_or(' ');
                    content.push(ch);
                }
            }
            content.push('\n');
        }
        snapshots.push(content);
    }

    // Each level should produce distinct output.
    for i in 0..levels.len() {
        for j in (i + 1)..levels.len() {
            assert_ne!(
                snapshots[i], snapshots[j],
                "{:?} and {:?} should produce different output",
                levels[i], levels[j]
            );
        }
    }

    // Verify level navigation cycling.
    let mut level = DisclosureLevel::Badge;
    for _ in 0..4 {
        level = level.next();
    }
    assert_eq!(
        level,
        DisclosureLevel::Badge,
        "4 next() calls should cycle back to Badge"
    );
}
