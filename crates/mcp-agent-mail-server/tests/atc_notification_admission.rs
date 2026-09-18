//! Real public ATC planning/admission regressions. These fixtures inspect the
//! effects handed to the executor, not a model of the decision engine. They do
//! not execute SQLite/archive effects or measure HTTP latency. Synthetic time
//! avoids sleeps; passive checks must never become outgoing mail, even in Live.

use mcp_agent_mail_core::{AtcExecutorMode, AtcWriteMode, Config};
use mcp_agent_mail_server::atc::{self, AtcSubsystem, AtcTickAction};
use std::collections::VecDeque;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());
const SECOND: i64 = 1_000_000;

fn reset_engine(fast_probes: bool, ledger_capacity: usize) -> tempfile::TempDir {
    let storage = tempfile::tempdir().expect("isolated ATC storage root");
    let config = Config {
        atc_enabled: true,
        atc_write_mode: AtcWriteMode::Live,
        atc_executor_mode: AtcExecutorMode::Live,
        atc_probe_interval_secs: if fast_probes { 1 } else { 120 },
        atc_ledger_capacity: ledger_capacity,
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
    matches!(family, "liveness_probe" | "liveness_monitoring" | "withheld_release_notice")
}

#[test]
fn live_recent_population_940_never_emits_routine_durable_liveness_mail() {
    const POPULATION: usize = 940;
    const TICKS: i64 = 1_200;
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(true, 1_000);
    enter_safe_mode();
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    let agents: Vec<_> = (0..POPULATION).map(|i| format!("HydratedAgent{i:04}")).collect();
    for agent in &agents {
        atc::atc_sync_agent_snapshot(agent, "codex-cli", Some("/admission-regression"), activity);
    }
    assert_eq!(atc::atc_summary().unwrap().tracked_agents.len(), POPULATION);
    // The existing executor's capacity/drain shape, not its database I/O.
    let mut pending = VecDeque::new();
    for tick in 0..TICKS {
        let now = activity + (120 + tick) * SECOND;
        if tick % 100 == 0 {
            for agent in &agents {
                let snapshot_ts = if tick % 200 == 0 { activity } else { activity - 1 };
                atc::atc_sync_agent_snapshot(agent, "codex-cli", Some("/admission-regression"), snapshot_ts);
            }
        }
        let report = atc::atc_tick_report(now).expect("enabled engine");
        assert!(report.summary.kernel.due_agents <= 8, "review batching regressed");
        assert_eq!(report.summary.kernel.pending_effects, report.effects.len());
        assert_eq!(atc::atc_summary().unwrap().kernel.pending_effects, report.effects.len());
        assert!(pending.len() + report.effects.len() <= 512, "queue overflow at tick {tick}");
        pending.extend((0..report.effects.len()).map(|_| ()));
        for _ in 0..64 {
            let _ = pending.pop_front();
        }
        assert!(report.effects.iter().all(|effect| !routine_liveness(&effect.semantics.family)));
        assert!(report.actions.is_empty(), "safe-mode liveness must remain passive");
    }
    assert!(pending.is_empty());
    let stats = atc::atc_delivery_stats();
    assert!(stats.passive_liveness > 0, "fixture must exercise real liveness proposals");
    assert_eq!(stats.admitted, 0);
    assert_eq!(stats.tracked_keys, 0, "passive identities must not accumulate delivery entries");
    assert_eq!(stats.capacity_suppressed, 0);
    let summary = atc::atc_summary().unwrap();
    assert!(summary.safe_mode);
    assert!(summary.decisions_total > 0, "inference and its evidence ledger must still run");
    for agent in &agents {
        assert_eq!(atc::atc_agent_last_activity(agent), Some(activity));
    }
}

#[test]
fn active_and_never_observed_agents_do_not_receive_probe_effects() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(false, 1_000);
    let start = mcp_agent_mail_core::timestamps::now_micros();
    for agent in ["BlueFox", "RedHawk"] {
        atc::atc_register_agent_with_project(agent, "codex-cli", Some("/admission-regression"));
    }
    for tick in 0..50 {
        let now = start + tick * SECOND;
        atc::atc_observe_activity_with_project("BlueFox", Some("/admission-regression"), now);
        let report = atc::atc_tick_report(now + 1_000).unwrap();
        assert!(report.effects.iter().all(|effect| effect.kind != "probe_agent"));
        assert!(report.actions.iter().all(|action| !matches!(action, AtcTickAction::ProbeAgent { .. })));
        assert_eq!(atc::atc_agent_last_activity("BlueFox"), Some(now));
        assert_eq!(atc::atc_agent_last_activity("RedHawk"), None);
    }
    let stats = atc::atc_delivery_stats();
    assert!(stats.recently_active > 0);
    assert!(stats.no_activity > 0);
    assert_eq!(stats.admitted, 0);
}

#[test]
fn action_only_api_never_converts_passive_checks_to_mail() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(true, 1_000);
    enter_safe_mode();
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    atc::atc_sync_agent_snapshot("BlueFox", "codex-cli", Some("/admission-regression"), activity);
    for tick in 0..200 {
        let actions = atc::atc_tick(activity + (2 + tick * 2) * SECOND);
        assert!(actions.is_empty(), "a proposed check is not a delivery entitlement");
    }
    assert!(atc::atc_delivery_stats().passive_liveness > 0);
    assert_eq!(atc::atc_agent_last_activity("BlueFox"), Some(activity));
}

#[test]
fn tiny_evidence_ledger_does_not_leave_orphaned_mail_actions() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _storage = reset_engine(true, 1);
    enter_safe_mode();
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    for index in 0..16 {
        atc::atc_sync_agent_snapshot(&format!("TinyLedgerAgent{index:02}"), "codex-cli", Some("/tiny-ledger"), activity);
    }
    for tick in 0..100 {
        let report = atc::atc_tick_report(activity + (600 + tick * 30) * SECOND).unwrap();
        assert!(report.effects.is_empty());
        assert!(report.actions.is_empty(), "evicted effect metadata allowed an orphaned mail action");
        assert!(report.summary.recent_decisions.len() <= 1);
    }
    assert!(atc::atc_summary().unwrap().decisions_total > 16);
    assert_eq!(atc::atc_delivery_stats().admitted, 0);
}
