//! Exercise the public planning/admission API with the real ATC engine.
//!
//! These tests inspect the effects handed to the executor; they do not claim
//! to measure SQLite writes or HTTP latency. Synthetic time avoids sleeps.

use mcp_agent_mail_core::{AtcExecutorMode, AtcWriteMode, Config};
use mcp_agent_mail_server::atc::{self, AtcSubsystem, AtcTickAction};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());
const SECOND: i64 = 1_000_000;

fn reset_engine(fast_probes: bool) -> tempfile::TempDir {
    let storage = tempfile::tempdir().expect("isolated ATC storage root");
    let config = Config {
        atc_enabled: true,
        atc_write_mode: AtcWriteMode::Live,
        atc_executor_mode: AtcExecutorMode::Live,
        atc_probe_interval_secs: if fast_probes { 1 } else { 120 },
        // Keep the overload fixture in the non-destructive posture even if
        // subsequent observations would ordinarily recover calibration.
        atc_safe_mode_recovery_count: 1_000_000,
        storage_root: storage.path().to_path_buf(),
        ..Config::default()
    };
    atc::init_global_atc(&config);
    storage
}

fn enter_safe_mode() {
    for _ in 0..64 {
        atc::atc_record_outcome(AtcSubsystem::Liveness, None, 1.0, 100.0, false);
    }
    assert!(atc::atc_summary().expect("initialized engine").safe_mode);
}

fn routine_liveness(family: &str) -> bool {
    matches!(
        family,
        "liveness_probe" | "liveness_monitoring" | "withheld_release_notice"
    )
}

#[test]
fn large_recent_population_and_repeated_snapshots_do_not_rearm_durable_prompts() {
    const POPULATION: usize = 940;
    const TICKS: i64 = 1_200;
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(true);
    enter_safe_mode();
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    let agents: Vec<_> = (0..POPULATION)
        .map(|index| format!("HydratedAgent{index:04}"))
        .collect();
    for agent in &agents {
        atc::atc_sync_agent_snapshot(
            agent,
            "codex-cli",
            Some("/admission-regression"),
            activity,
        );
    }
    assert_eq!(atc::atc_summary().unwrap().tracked_agents.len(), POPULATION);

    let mut emitted_by_key = HashMap::<String, usize>::new();
    // Model the existing executor's capacity/drain shape, not its database I/O.
    let mut pending = VecDeque::new();
    for tick in 0..TICKS {
        let now = activity + (120 + tick) * SECOND;
        if tick % 100 == 0 {
            for agent in &agents {
                let snapshot_ts = if tick % 200 == 0 {
                    activity
                } else {
                    activity - 1
                };
                atc::atc_sync_agent_snapshot(
                    agent,
                    "codex-cli",
                    Some("/admission-regression"),
                    snapshot_ts,
                );
            }
        }
        let report = atc::atc_tick_report(now).expect("enabled engine");
        assert!(
            report.summary.kernel.due_agents <= 8,
            "post-v0.3.36 batching regressed"
        );
        assert_eq!(report.summary.kernel.pending_effects, report.effects.len());
        assert_eq!(
            atc::atc_summary().unwrap().kernel.pending_effects,
            report.effects.len(),
            "standalone summary must expose the admitted, not raw, effect count"
        );
        assert!(
            pending.len() + report.effects.len() <= 512,
            "admitted effects overflowed the executor queue on tick {tick}"
        );
        pending.extend((0..report.effects.len()).map(|_| ()));
        for _ in 0..64 {
            let _ = pending.pop_front();
        }
        for effect in &report.effects {
            if routine_liveness(&effect.semantics.family) {
                let count = emitted_by_key
                    .entry(effect.semantics.cooldown_key.clone())
                    .or_default();
                *count += 1;
                assert_eq!(
                    *count, 1,
                    "an unchanged activity epoch produced another {} for {}",
                    effect.semantics.family, effect.agent
                );
                assert_eq!(atc::atc_agent_last_activity(&effect.agent), Some(activity));
            }
        }
    }
    assert!(pending.is_empty());
    assert!(
        !emitted_by_key.is_empty(),
        "fixture must actually emit notifications"
    );
    let stats = atc::atc_delivery_stats();
    assert!(stats.duplicate > 0, "fixture must exercise repeated proposals");
    assert_eq!(stats.capacity_suppressed, 0);
    assert!(stats.tracked_keys <= 16_384);
    assert!(atc::atc_summary().unwrap().safe_mode);
    for agent in &agents {
        assert_eq!(atc::atc_agent_last_activity(agent), Some(activity));
    }
}

#[test]
fn active_and_never_observed_agents_do_not_receive_probe_effects() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(false);
    let start = mcp_agent_mail_core::timestamps::now_micros();
    atc::atc_register_agent_with_project(
        "BlueFox",
        "codex-cli",
        Some("/admission-regression"),
    );
    atc::atc_register_agent_with_project(
        "RedHawk",
        "codex-cli",
        Some("/admission-regression"),
    );
    for tick in 0..50 {
        let now = start + tick * SECOND;
        atc::atc_observe_activity_with_project(
            "BlueFox",
            Some("/admission-regression"),
            now,
        );
        let report = atc::atc_tick_report(now + 1_000).expect("enabled engine");
        assert!(
            report.effects.iter().all(|effect| effect.kind != "probe_agent")
        );
        assert!(
            report.actions.iter().all(|action| !matches!(
                action,
                AtcTickAction::ProbeAgent { .. }
            ))
        );
        assert_eq!(atc::atc_agent_last_activity("BlueFox"), Some(now));
        assert_eq!(atc::atc_agent_last_activity("RedHawk"), None);
    }
    let stats = atc::atc_delivery_stats();
    assert!(
        stats.recently_active > 0,
        "fixture must exercise active probe suppression"
    );
    assert!(
        stats.no_activity > 0,
        "fixture must exercise unobserved probe suppression"
    );
}

#[test]
fn action_only_tick_api_cannot_bypass_unanswered_probe_coalescing() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(true);
    enter_safe_mode();
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    atc::atc_sync_agent_snapshot(
        "BlueFox",
        "codex-cli",
        Some("/admission-regression"),
        activity,
    );
    let mut probes = 0;
    for tick in 0..200 {
        for action in atc::atc_tick(activity + (2 + tick * 2) * SECOND) {
            if matches!(
                action,
                AtcTickAction::ProbeAgent { ref agent } if agent == "BlueFox"
            ) {
                probes += 1;
            }
        }
    }
    assert_eq!(
        probes, 1,
        "fresh decisions are not fresh delivery entitlements"
    );
    assert_eq!(atc::atc_agent_last_activity("BlueFox"), Some(activity));
}
