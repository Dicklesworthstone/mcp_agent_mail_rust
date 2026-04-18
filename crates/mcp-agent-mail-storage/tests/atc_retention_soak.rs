//! Soak coverage for ATC retention, replay, and rollup preservation.
//!
//! This exercises the file-backed ATC APIs through a 7-day synthetic workload
//! without touching the ATC TUI surface. The scenario intentionally straddles
//! the raw-row retention boundary so the test can prove that:
//! - old terminal rows compact away,
//! - open rows remain queryable,
//! - replay stays sequence-stable after compaction,
//! - rollups survive raw-row deletion unchanged.

use std::time::{SystemTime, UNIX_EPOCH};

use asupersync::runtime::RuntimeBuilder;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::atc_retention::{LearningArtifactKind, retention_rule};
use mcp_agent_mail_core::{
    EffectKind, ExperienceOutcome, ExperienceRow, ExperienceState, ExperienceSubsystem,
};
use mcp_agent_mail_db::atc_queries::{
    OpenExperienceFilter, SequenceRange, query_open_experiences, query_rollups, refresh_rollups,
    replay, retention_compact,
};
use mcp_agent_mail_db::queries::insert_experience;
use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig, RollupEntry, schema};
use tempfile::TempDir;

const MICROS_PER_DAY: i64 = 86_400_000_000;
const SIMULATED_DAYS: usize = 7;
const DAILY_TERMINAL_ROWS: usize = 4;
const DAILY_OPEN_ROWS: usize = 2;
const DAILY_TOTAL_ROWS: usize = DAILY_TERMINAL_ROWS + DAILY_OPEN_ROWS;

#[derive(Clone, Copy)]
struct InsertedMeta {
    experience_id: u64,
    state: ExperienceState,
    created_ts_micros: i64,
    resolved_ts_micros: Option<i64>,
}

fn now_micros() -> i64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_micros();
    i64::try_from(micros).unwrap_or(i64::MAX)
}

fn test_pool(tmp: &TempDir) -> DbPool {
    let db_path = tmp.path().join("atc_retention_soak.db");
    DbPool::new(&DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(tmp.path().join("storage")),
        max_connections: 1,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 60_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: schema::DEFAULT_CACHE_BUDGET_KB,
    })
    .expect("create file-backed ATC test pool")
}

fn expect_outcome<T>(outcome: Outcome<T, DbError>, context: &str) -> T {
    match outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(error) => panic!("{context}: {error}"),
        Outcome::Cancelled(reason) => panic!("{context} cancelled: {reason:?}"),
        Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
    }
}

fn action_for(effect_kind: EffectKind) -> &'static str {
    match effect_kind {
        EffectKind::Advisory => "SendAdvisory",
        EffectKind::Probe => "ProbeAgent",
        EffectKind::Release => "ReleaseReservation",
        EffectKind::ForceReservation => "ForceReservation",
        EffectKind::RoutingSuggestion => "SuggestRoute",
        EffectKind::Backpressure => "ApplyBackpressure",
        EffectKind::NoAction => "NoAction",
    }
}

fn scenario_row(
    day_index: usize,
    ordinal: usize,
    state: ExperienceState,
    subsystem: ExperienceSubsystem,
    effect_kind: EffectKind,
    created_ts_micros: i64,
) -> ExperienceRow {
    let decision_id = ((day_index as u64) * 100) + ordinal as u64 + 1;
    let effect_id = decision_id + 10_000;
    let dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
    let executed_ts_micros = match state {
        ExperienceState::Resolved
        | ExperienceState::Censored
        | ExperienceState::Expired
        | ExperienceState::Open => Some(created_ts_micros.saturating_add(2_000)),
        _ => None,
    };
    let resolved_ts_micros = match state {
        ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired => {
            Some(created_ts_micros.saturating_add(20_000))
        }
        _ => None,
    };
    let outcome = match state {
        ExperienceState::Resolved => {
            let correct = day_index.is_multiple_of(2) == ordinal.is_multiple_of(2);
            Some(ExperienceOutcome {
                observed_ts_micros: resolved_ts_micros.expect("resolved timestamp"),
                label: format!("resolved-day-{day_index}-ordinal-{ordinal}"),
                correct,
                actual_loss: Some(if correct { 0.1 } else { 1.2 }),
                regret: Some(if correct { 0.0 } else { 0.3 }),
                evidence: Some(serde_json::json!({
                    "simulated_day": day_index,
                    "ordinal": ordinal,
                    "state": state.to_string(),
                })),
            })
        }
        _ => None,
    };

    ExperienceRow {
        experience_id: 0,
        decision_id,
        effect_id,
        trace_id: format!("trc-soak-{day_index}-{ordinal}"),
        claim_id: format!("clm-soak-{day_index}-{ordinal}"),
        evidence_id: format!("evi-soak-{day_index}-{ordinal}"),
        state,
        subsystem,
        decision_class: "retention_soak".to_string(),
        subject: format!("SoakAgent{:02}", (day_index * 10) + ordinal),
        project_key: Some("/tmp/atc-retention-soak".to_string()),
        policy_id: Some("liveness-incumbent-r1".to_string()),
        effect_kind,
        action: action_for(effect_kind).to_string(),
        posterior: vec![
            ("Alive".to_string(), 0.55),
            ("Flaky".to_string(), 0.25),
            ("Dead".to_string(), 0.20),
        ],
        expected_loss: 0.75 + ordinal as f64,
        runner_up_action: Some("FallbackAction".to_string()),
        runner_up_loss: Some(1.5 + ordinal as f64),
        evidence_summary: format!("7-day retention soak day {day_index} ordinal {ordinal}"),
        calibration_healthy: true,
        safe_mode_active: false,
        non_execution_reason: None,
        outcome,
        created_ts_micros,
        dispatched_ts_micros,
        executed_ts_micros,
        resolved_ts_micros,
        features: None,
        feature_ext: None,
        context: Some(serde_json::json!({
            "simulated_day": day_index,
            "ordinal": ordinal,
            "effect_kind": effect_kind.to_string(),
            "state": state.to_string(),
        })),
    }
}

fn day_rows(day_index: usize, day_anchor_micros: i64) -> Vec<ExperienceRow> {
    vec![
        scenario_row(
            day_index,
            0,
            ExperienceState::Resolved,
            ExperienceSubsystem::Liveness,
            EffectKind::Probe,
            day_anchor_micros + 1_000_000,
        ),
        scenario_row(
            day_index,
            1,
            ExperienceState::Resolved,
            ExperienceSubsystem::Conflict,
            EffectKind::Release,
            day_anchor_micros + 2_000_000,
        ),
        scenario_row(
            day_index,
            2,
            ExperienceState::Censored,
            ExperienceSubsystem::Synthesis,
            EffectKind::Advisory,
            day_anchor_micros + 3_000_000,
        ),
        scenario_row(
            day_index,
            3,
            ExperienceState::Expired,
            ExperienceSubsystem::Conflict,
            EffectKind::Release,
            day_anchor_micros + 4_000_000,
        ),
        scenario_row(
            day_index,
            4,
            ExperienceState::Open,
            ExperienceSubsystem::Liveness,
            EffectKind::Probe,
            day_anchor_micros + 5_000_000,
        ),
        scenario_row(
            day_index,
            5,
            ExperienceState::Open,
            ExperienceSubsystem::Conflict,
            EffectKind::Release,
            day_anchor_micros + 6_000_000,
        ),
    ]
}

fn rollup_total_count(rows: &[RollupEntry]) -> i64 {
    rows.iter().map(|row| row.total_count).sum()
}

#[test]
fn atc_retention_compaction_soak_preserves_rollups_and_open_rows() {
    let tmp = TempDir::new().expect("create tempdir");
    let pool = test_pool(&tmp);
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let cx = Cx::for_testing();

    let resolved_rule =
        retention_rule(LearningArtifactKind::ResolvedExperienceRows).expect("resolved rule");
    let max_age_micros = i64::from(
        resolved_rule
            .compact_after_days
            .unwrap_or(resolved_rule.hot_days),
    ) * MICROS_PER_DAY;
    let now = now_micros();
    let start_day_micros = now
        .saturating_sub(max_age_micros)
        .saturating_sub(5 * MICROS_PER_DAY);
    let lookback_micros = now
        .saturating_sub(start_day_micros)
        .saturating_add(MICROS_PER_DAY);

    let mut inserted = Vec::with_capacity(SIMULATED_DAYS * DAILY_TOTAL_ROWS);

    for day_index in 0..SIMULATED_DAYS {
        let day_anchor_micros =
            start_day_micros.saturating_add((day_index as i64) * MICROS_PER_DAY);
        for row in day_rows(day_index, day_anchor_micros) {
            let assigned_id = expect_outcome(
                runtime.block_on(insert_experience(&cx, &pool, row.clone())),
                "insert synthetic ATC experience",
            );
            inserted.push(InsertedMeta {
                experience_id: assigned_id,
                state: row.state,
                created_ts_micros: row.created_ts_micros,
                resolved_ts_micros: row.resolved_ts_micros,
            });
        }

        let replayed = expect_outcome(
            runtime.block_on(replay(&cx, &pool, SequenceRange::default())),
            "replay during soak accumulation",
        );
        assert_eq!(replayed.rows.len(), inserted.len());
        assert!(
            replayed
                .rows
                .windows(2)
                .all(|window| window[0].experience_id < window[1].experience_id),
            "replay should remain sequence-ordered during accumulation"
        );

        let open_rows = expect_outcome(
            runtime.block_on(query_open_experiences(
                &cx,
                &pool,
                OpenExperienceFilter::default(),
            )),
            "query open experiences during soak accumulation",
        );
        assert_eq!(open_rows.len(), (day_index + 1) * DAILY_OPEN_ROWS);
        assert!(
            open_rows
                .iter()
                .all(|row| row.state == ExperienceState::Open)
        );

        expect_outcome(
            runtime.block_on(refresh_rollups(&cx, &pool, now, lookback_micros)),
            "refresh rollups during soak accumulation",
        );
        let rollups = expect_outcome(
            runtime.block_on(query_rollups(&cx, &pool)),
            "query rollups during soak accumulation",
        );
        assert_eq!(rollup_total_count(&rollups) as usize, inserted.len());
    }

    let open_before = expect_outcome(
        runtime.block_on(query_open_experiences(
            &cx,
            &pool,
            OpenExperienceFilter::default(),
        )),
        "query open experiences before compaction",
    );
    let rollups_before = expect_outcome(
        runtime.block_on(query_rollups(&cx, &pool)),
        "query rollups before compaction",
    );

    let compact_summary = expect_outcome(
        runtime.block_on(retention_compact(&cx, &pool, max_age_micros, true)),
        "compact retained ATC rows",
    );
    let expected_deleted_rows = inserted
        .iter()
        .filter(|row| {
            matches!(
                row.state,
                ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired
            ) && row.resolved_ts_micros.unwrap_or(i64::MAX) <= compact_summary.cutoff_ts_micros
        })
        .count();
    let expected_remaining_ids = inserted
        .iter()
        .filter(|row| {
            !matches!(
                row.state,
                ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired
            ) || row.resolved_ts_micros.unwrap_or(i64::MAX) > compact_summary.cutoff_ts_micros
        })
        .map(|row| row.experience_id)
        .collect::<Vec<_>>();

    assert_eq!(compact_summary.deleted_rows, expected_deleted_rows);
    assert!(compact_summary.preserved_rollups);

    let replayed_after = expect_outcome(
        runtime.block_on(replay(&cx, &pool, SequenceRange::default())),
        "replay after compaction",
    );
    let replayed_after_ids = replayed_after
        .rows
        .iter()
        .map(|row| row.experience_id)
        .collect::<Vec<_>>();
    assert_eq!(replayed_after_ids, expected_remaining_ids);
    assert!(
        replayed_after
            .rows
            .windows(2)
            .all(|window| window[0].experience_id < window[1].experience_id),
        "replay should remain sequence-ordered after compaction"
    );
    assert_eq!(
        replayed_after
            .rows
            .iter()
            .filter(|row| row.state == ExperienceState::Open)
            .count(),
        SIMULATED_DAYS * DAILY_OPEN_ROWS
    );
    assert_eq!(
        replayed_after
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.state,
                    ExperienceState::Resolved
                        | ExperienceState::Censored
                        | ExperienceState::Expired
                )
            })
            .count(),
        DAILY_TERMINAL_ROWS
    );

    let open_after = expect_outcome(
        runtime.block_on(query_open_experiences(
            &cx,
            &pool,
            OpenExperienceFilter::default(),
        )),
        "query open experiences after compaction",
    );
    assert_eq!(open_after.len(), SIMULATED_DAYS * DAILY_OPEN_ROWS);
    assert_eq!(
        open_before
            .iter()
            .map(|row| row.experience_id)
            .collect::<Vec<_>>(),
        open_after
            .iter()
            .map(|row| row.experience_id)
            .collect::<Vec<_>>(),
        "compaction must not disturb open rows"
    );
    assert!(
        open_after
            .windows(2)
            .all(|window| window[0].created_ts_micros >= window[1].created_ts_micros),
        "query_open_experiences should stay sorted newest-first"
    );

    let rollups_after = expect_outcome(
        runtime.block_on(query_rollups(&cx, &pool)),
        "query rollups after compaction",
    );
    assert_eq!(rollups_before, rollups_after);
    assert_eq!(
        rollup_total_count(&rollups_after) as usize,
        SIMULATED_DAYS * DAILY_TOTAL_ROWS
    );

    let second_pass = expect_outcome(
        runtime.block_on(retention_compact(&cx, &pool, max_age_micros, true)),
        "run idempotent compaction pass",
    );
    assert_eq!(second_pass.deleted_rows, 0);

    let oldest_open_created_ts = inserted
        .iter()
        .filter(|row| row.state == ExperienceState::Open)
        .map(|row| row.created_ts_micros)
        .min()
        .expect("oldest open created_ts");
    assert!(
        oldest_open_created_ts <= compact_summary.cutoff_ts_micros,
        "scenario should include open rows older than the raw-row compaction cutoff"
    );
}
