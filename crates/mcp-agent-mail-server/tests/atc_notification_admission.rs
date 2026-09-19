//! Real public ATC planning/admission regressions. These fixtures inspect the
//! effects handed to the executor, not a model of the decision engine. Population
//! fixtures also use the production database, migrations, pool, and query. They
//! do not execute outgoing mailbox/archive effects or measure HTTP latency.
//! Synthetic time avoids sleeps; passive checks stay non-mailing even in Live.

use mcp_agent_mail_core::{AtcWriteMode, Config};
use mcp_agent_mail_server::atc::{self, AtcSubsystem, AtcTickAction};
use std::collections::VecDeque;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());
const SECOND: i64 = 1_000_000;

fn reset_engine(fast_probes: bool, ledger_capacity: usize) -> tempfile::TempDir {
    let storage = tempfile::tempdir().expect("isolated ATC storage root");
    let config = Config {
        atc_enabled: true,
        // Executor mode is no longer a `Config` field: it resolves from
        // `AM_ATC_EXECUTOR_MODE` and is consumed by the server layer, not by the
        // `atc::` tick API these fixtures exercise. `atc_write_mode` is what makes
        // this a Live-mode regression.
        atc_write_mode: AtcWriteMode::Live,
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

// The following fixtures use the production DbConn backend, schema migrations,
// pool, population query, and public ATC API. No model substitutes for the DB
// projection or engine update path; outgoing effects still are not executed.
fn population_database(
    root: &std::path::Path,
    count: usize,
    activity: i64,
) -> (mcp_agent_mail_db::DbPool, std::path::PathBuf) {
    let path = root.join("population-hydration.db");
    let conn = mcp_agent_mail_db::DbConn::open_file(path.display().to_string()).unwrap();
    conn.execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL).unwrap();
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base()).unwrap();
    let cx = asupersync::Cx::for_testing();
    match fastmcp_core::block_on(mcp_agent_mail_db::schema::migrate_to_latest_base(&cx, &conn)) {
        asupersync::Outcome::Ok(_) => {}
        other => panic!("initialize population fixture: {other:?}"),
    }
    // Seed in one transaction: exercise the real query without turning setup
    // into 940 registration durability probes and archive/index updates.
    conn.execute_raw("BEGIN").unwrap();
    conn.execute_raw(&format!(
        "INSERT INTO projects (id, slug, human_key, created_at) \
         VALUES (1, 'population-hydration', '/population-hydration', {activity})"
    )).unwrap();
    for index in 0..count {
        conn.execute_raw(&format!(
            "INSERT INTO agents \
             (project_id, name, program, model, task_description, inception_ts, last_active_ts, \
              attachments_policy, contact_policy, reaper_exempt) \
             VALUES (1, 'DatabaseAgent{index:04}', 'codex-cli', 'fixture', '', \
                     {activity}, {activity}, 'auto', 'auto', 0)"
        )).unwrap();
    }
    conn.execute_raw("COMMIT").unwrap();
    drop(conn);
    let pool = mcp_agent_mail_db::create_pool_without_startup_init(&mcp_agent_mail_db::DbPoolConfig {
        database_url: format!("sqlite:///{}", path.display()),
        min_connections: 1,
        max_connections: 1,
        run_migrations: false,
        warmup_connections: 0,
        ..Default::default()
    }).unwrap();
    (pool, path)
}

#[test]
fn database_population_940_uses_bounded_hydration_and_noop_steady_state_refreshes() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    mcp_agent_mail_core::config::with_process_env_overrides_for_test(
        &[("AM_ATC_POPULATION_LIMIT", "4096"), ("AM_ATC_POPULATION_RECENCY_SECS", "604800")],
        || {
            let storage = reset_engine(false, 1_000);
            let activity = mcp_agent_mail_core::timestamps::now_micros();
            let (pool, path) = population_database(storage.path(), 940, activity);
            let tracker = std::sync::Arc::new(mcp_agent_mail_db::QueryTracker::new());
            tracker.enable(None);
            let _query_guard = mcp_agent_mail_db::tracking::set_active_tracker(tracker.clone());

            let snapshot = atc::atc_sync_population_from_db(&pool).unwrap();
            assert_eq!(snapshot.projects, 1);
            assert_eq!(snapshot.agents, 940);
            assert_eq!(snapshot.active_agents, 940);
            assert_eq!(tracker.snapshot().total, 1, "snapshot must remain one joined query");
            let first = atc::atc_population_hydration_stats();
            assert!((1..=32).contains(&first.applied_agents));
            assert_eq!(first.applied_agents + first.pending_agents, 940);
            assert_eq!(atc::atc_summary().unwrap().tracked_agents.len(), first.applied_agents);

            for _ in 0..5 {
                assert_eq!(atc::atc_sync_population_from_db(&pool).unwrap(), snapshot);
            }
            assert_eq!(tracker.snapshot().total, 1, "unfinished hydration must not re-query");
            assert_eq!(atc::atc_population_hydration_stats().deferred_refreshes, 5);

            // A real attributed tool observation arrives before its older DB
            // snapshot is applied. The engine must not lose that newer signal.
            let observed = activity + 5 * SECOND;
            atc::atc_observe_activity_with_project(
                "DatabaseAgent0000", Some("/population-hydration"), observed,
            );
            let mut previous = atc::atc_population_hydration_stats().applied_agents;
            let mut slices = 0;
            while atc::atc_population_hydration_stats().pending_agents > 0 {
                let now = activity + 10 * SECOND;
                let report = atc::atc_tick_report(now).unwrap();
                let progress = atc::atc_population_hydration_stats();
                assert!((1..=32).contains(&(progress.applied_agents - previous)));
                assert_eq!(progress.applied_agents + progress.pending_agents, 940);
                assert_eq!(report.summary.completeness, atc::SnapshotCompleteness::Partial);
                assert_eq!(report.summary.kernel.next_due_micros, Some(now));
                assert_eq!(report.summary.tick_count, 0, "inference ran on partial DB evidence");
                assert!(report.effects.is_empty());
                assert!(report.actions.is_empty());
                previous = progress.applied_agents;
                slices += 1;
                assert!(slices <= 940, "population hydration stopped making progress");
            }
            assert_eq!(tracker.snapshot().total, 1, "hydration slices must be DB-free");
            assert_eq!(atc::atc_agent_last_activity("DatabaseAgent0000"), Some(observed));
            let report = atc::atc_tick_report(activity + 10 * SECOND).unwrap();
            assert_eq!(report.summary.completeness, atc::SnapshotCompleteness::Full);
            assert_eq!(report.summary.tick_count, 1);
            assert_eq!(report.summary.tracked_agents.len(), 940);

            assert_eq!(atc::atc_sync_population_from_db(&pool).unwrap(), snapshot);
            assert_eq!(tracker.snapshot().total, 2);
            let warm = atc::atc_population_hydration_stats();
            assert_eq!(warm.unchanged_agents, 940);
            assert_eq!(warm.applied_agents, 0);
            assert_eq!(warm.pending_agents, 0);
            assert_eq!(atc::atc_agent_last_activity("DatabaseAgent0000"), Some(observed));

            // A changed DB row must not be hidden by the unchanged-row cache.
            let fresh_activity = activity + 50 * SECOND;
            let conn = mcp_agent_mail_db::DbConn::open_file(path.display().to_string()).unwrap();
            conn.execute_raw(&format!(
                "UPDATE agents SET last_active_ts = {fresh_activity}, program = 'claude-code' \
                 WHERE name = 'DatabaseAgent0000'"
            )).unwrap();
            drop(conn);
            let before_refresh_queries = tracker.snapshot().total;
            assert_eq!(atc::atc_sync_population_from_db(&pool).unwrap(), snapshot);
            assert_eq!(tracker.snapshot().total - before_refresh_queries, 1);
            let changed = atc::atc_population_hydration_stats();
            assert_eq!(changed.unchanged_agents, 939);
            assert_eq!(changed.applied_agents, 1);
            assert_eq!(changed.pending_agents, 0);
            assert_eq!(atc::atc_agent_last_activity("DatabaseAgent0000"), Some(fresh_activity));
        },
    );
}

#[test]
fn disabled_population_sync_is_read_free_and_small_enabled_snapshot_remains_immediate() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let storage = reset_engine(false, 1_000);
    let activity = mcp_agent_mail_core::timestamps::now_micros();
    let (pool, _) = population_database(storage.path(), 1, activity);
    let mut config = Config {
        atc_enabled: false,
        storage_root: storage.path().to_path_buf(),
        ..Config::default()
    };
    atc::init_global_atc(&config);
    let tracker = std::sync::Arc::new(mcp_agent_mail_db::QueryTracker::new());
    tracker.enable(None);
    let _query_guard = mcp_agent_mail_db::tracking::set_active_tracker(tracker.clone());
    assert_eq!(atc::atc_sync_population_from_db(&pool).unwrap(), atc::AtcPopulationSyncStats::default());
    assert_eq!(tracker.snapshot().total, 0);
    assert_eq!(atc::atc_population_hydration_stats(), atc::AtcPopulationHydrationStats::default());

    config.atc_enabled = true;
    atc::init_global_atc(&config);
    assert_eq!(atc::atc_sync_population_from_db(&pool).unwrap().agents, 1);
    assert_eq!(tracker.snapshot().total, 1);
    assert_eq!(atc::atc_population_hydration_stats().pending_agents, 0);
    assert_eq!(atc::atc_agent_last_activity("DatabaseAgent0000"), Some(activity));
    assert_eq!(atc::atc_tick_report(activity + SECOND).unwrap().summary.completeness, atc::SnapshotCompleteness::Full);
}
