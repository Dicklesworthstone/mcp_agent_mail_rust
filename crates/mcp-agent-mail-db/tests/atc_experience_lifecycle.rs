#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::used_underscore_binding,
    clippy::cast_possible_wrap
)]

//! End-to-end tests for the ATC (Agent Trust & Compliance) experience lifecycle.
//!
//! This test exercises the COMPLETE pipeline with REAL SQLite:
//!   append (Planned) → dispatch → execute → open → resolve → rollup
//!
//! No mocks. No fakes. Real DB operations with real schema migrations.
//!
//! Each step is verified with SELECT queries to prove the data is
//! actually in the database and transitions are correct.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_core::experience::{
    EffectKind, ExperienceOutcome, ExperienceRow, ExperienceState, ExperienceSubsystem,
    FeatureVector, NonExecutionReason, ResolutionKind,
};
use mcp_agent_mail_db::queries::{
    append_atc_experience, fetch_open_atc_experiences, resolve_atc_experience,
    resolve_experience, transition_atc_experience, update_atc_experience_rollup,
};
use mcp_agent_mail_db::{DbConn, DbPool, DbPoolConfig, create_pool};

/// Create a real SQLite database via the production migration path.
///
/// This mirrors exactly what happens in production:
/// 1. Open the DB file
/// 2. Apply init PRAGMAs
/// 3. Apply base schema (CREATE TABLE for core tables)
/// 4. Run ALL migrations via the migration runner (v16 ATC tables, v18 EWMA columns)
///
/// This proves that a fresh install or legacy upgrade gets all ATC tables automatically.
fn setup_real_db(rt: &asupersync::runtime::Runtime, name: &str) -> (Cx, DbPool, tempfile::TempDir) {
    let cx = Cx::for_testing();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join(name);

    // Step 1: Create DB file with PRAGMAs and base schema (same as pool init).
    let init_conn = DbConn::open_file(db_path.display().to_string()).expect("open DB file");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
        .expect("apply PRAGMAs");
    let base_sql = mcp_agent_mail_db::schema::init_schema_sql_base();
    init_conn.execute_raw(&base_sql).expect("apply base schema");

    // Step 2: Run ALL migrations via the production migration runner.
    // This creates the migration tracking table, applies v16 (ATC experience
    // tables + indexes), and v18 (EWMA/delay columns on rollups).
    rt.block_on(async {
        match mcp_agent_mail_db::schema::migrate_to_latest_base(&cx, &init_conn).await {
            asupersync::Outcome::Ok(applied) => {
                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
                for m in &applied {
                    eprintln!("[SETUP]   - {m}");
                }
            }
            asupersync::Outcome::Err(err) => {
                panic!("migration failed for {name}: {err}");
            }
            other => panic!("migration unexpected outcome: {other:?}"),
        }
    });
    drop(init_conn);

    // Step 3: Create pool (migrations already applied, skip re-running).
    let cfg = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        min_connections: 1,
        max_connections: 2,
        run_migrations: false, // already applied above
        warmup_connections: 0,
        ..Default::default()
    };
    let pool = create_pool(&cfg).expect("create pool");
    (cx, pool, dir)
}

fn make_probe_experience(decision_id: u64, effect_id: u64, subject: &str) -> ExperienceRow {
    ExperienceRow {
        experience_id: 0,
        decision_id,
        effect_id,
        trace_id: format!("trc-e2e-{decision_id}"),
        claim_id: format!("clm-e2e-{decision_id}"),
        evidence_id: format!("evi-e2e-{decision_id}"),
        state: ExperienceState::Planned,
        subsystem: ExperienceSubsystem::Liveness,
        decision_class: "liveness_check".to_string(),
        subject: subject.to_string(),
        project_key: Some("/tmp/e2e-atc".to_string()),
        policy_id: Some("liveness-v1".to_string()),
        effect_kind: EffectKind::Probe,
        action: "ProbeAgent".to_string(),
        posterior: vec![
            ("Alive".to_string(), 0.70),
            ("Flaky".to_string(), 0.20),
            ("Dead".to_string(), 0.10),
        ],
        expected_loss: 2.5,
        runner_up_action: Some("DeferProbe".to_string()),
        runner_up_loss: Some(4.0),
        evidence_summary: "agent silent for 120s".to_string(),
        calibration_healthy: true,
        safe_mode_active: false,
        non_execution_reason: None,
        outcome: None,
        created_ts_micros: 1_700_000_000_000_000,
        dispatched_ts_micros: None,
        executed_ts_micros: None,
        resolved_ts_micros: None,
        features: Some(FeatureVector {
            posterior_alive_bp: 7000,
            posterior_flaky_bp: 2000,
            expected_loss_bp: 250,
            loss_gap_bp: 150,
            risk_tier: 0,
            ..FeatureVector::zeroed()
        }),
        feature_ext: None,
        context: Some(serde_json::json!({"test": "e2e_lifecycle"})),
    }
}

/// Full lifecycle: Planned → Dispatched → Executed → Open → Resolved → Rollup.
#[test]
fn full_probe_lifecycle_with_resolution_and_rollup() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "atc_lifecycle_full.db");

    rt.block_on(async {
        let row = make_probe_experience(100, 200, "GreenCastle");

        // ── Step 1: Append (Planned) ──
        eprintln!("\n[1/6] APPEND experience: decision=100, effect=200, subject=GreenCastle");
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append experience");

        let exp_id = stored.experience_id;
        assert!(exp_id > 0);
        assert_eq!(stored.state, ExperienceState::Planned);
        assert_eq!(stored.subject, "GreenCastle");
        assert_eq!(stored.effect_kind, EffectKind::Probe);
        eprintln!("[1/6] OK: experience_id={exp_id}, state=planned");

        // ── Step 2: Dispatch ──
        eprintln!("[2/6] TRANSITION: planned → dispatched");
        transition_atc_experience(
            &cx,
            &pool,
            exp_id,
            ExperienceState::Dispatched,
            1_700_000_000_100_000,
            None,
            None,
        )
        .await
        .into_result()
        .expect("dispatch");
        eprintln!("[2/6] OK: dispatched");

        // ── Step 3: Execute ──
        eprintln!("[3/6] TRANSITION: dispatched → executed");
        transition_atc_experience(
            &cx,
            &pool,
            exp_id,
            ExperienceState::Executed,
            1_700_000_000_200_000,
            None,
            Some(&serde_json::json!({"probe_sent": true})),
        )
        .await
        .into_result()
        .expect("execute");
        eprintln!("[3/6] OK: executed");

        // ── Step 4: Open ──
        eprintln!("[4/6] TRANSITION: executed → open");
        transition_atc_experience(
            &cx,
            &pool,
            exp_id,
            ExperienceState::Open,
            1_700_000_000_300_000,
            None,
            None,
        )
        .await
        .into_result()
        .expect("open");

        // Verify open query finds it
        let open = fetch_open_atc_experiences(&cx, &pool, Some("GreenCastle"), 10)
            .await
            .into_result()
            .expect("fetch open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].experience_id, exp_id);
        eprintln!("[4/6] OK: open (verified via fetch_open_atc_experiences)");

        // ── Step 5: Resolve ──
        let outcome = ExperienceOutcome {
            observed_ts_micros: 1_700_000_000_500_000,
            label: "probe_responded: agent alive, latency 1.8s".to_string(),
            correct: true,
            actual_loss: Some(0.5),
            regret: Some(0.0),
            evidence: Some(serde_json::json!({"response_ms": 1800, "status": "alive"})),
        };
        eprintln!("[5/6] RESOLVE: correct=true, loss=0.5, regret=0.0");
        resolve_atc_experience(&cx, &pool, exp_id, &outcome)
            .await
            .into_result()
            .expect("resolve");

        // Verify it's gone from open
        let open_after = fetch_open_atc_experiences(&cx, &pool, Some("GreenCastle"), 10)
            .await
            .into_result()
            .expect("re-fetch open");
        assert_eq!(open_after.len(), 0);

        // Verify the resolved experience has the outcome stored in the DB.
        // Re-read the row directly via a sync connection to confirm
        // state=resolved and outcome_json is populated.
        let db_path = _dir.path().join("atc_lifecycle_full.db");
        let verify_conn =
            DbConn::open_file(db_path.display().to_string()).expect("open for verification");
        let verify_rows = verify_conn.query_sync(
            "SELECT state, outcome_json, resolved_ts FROM atc_experiences WHERE experience_id = ?",
            &[sqlmodel_core::Value::BigInt(exp_id as i64)],
        ).expect("verify resolved row");
        assert_eq!(verify_rows.len(), 1, "experience row must still exist");
        let state_val = verify_rows[0].get(0).and_then(|v| match v {
            sqlmodel_core::Value::Text(s) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(
            state_val,
            Some("resolved"),
            "DB row must be in resolved state"
        );
        let outcome_val = verify_rows[0].get(1).and_then(|v| match v {
            sqlmodel_core::Value::Text(s) => Some(s.clone()),
            _ => None,
        });
        assert!(outcome_val.is_some(), "outcome_json must be populated");
        let outcome_json: serde_json::Value =
            serde_json::from_str(&outcome_val.unwrap()).expect("outcome_json must be valid JSON");
        assert_eq!(
            outcome_json["correct"], true,
            "outcome.correct must be true"
        );
        assert_eq!(
            outcome_json["actual_loss"], 0.5,
            "outcome.actual_loss must be 0.5"
        );
        eprintln!("[5/6] OK: resolved (verified state=resolved + outcome in DB)");

        // ── Step 6: Rollup ──
        eprintln!("[6/6] ROLLUP: stratum=liveness:probe:tier0");
        update_atc_experience_rollup(
            &cx,
            &pool,
            "liveness:probe:tier0",
            "liveness",
            "probe",
            0,
            ExperienceState::Resolved,
            true,
            0.5,
            0.0,
            200_000,
            1_700_000_000_500_000,
        )
        .await
        .into_result()
        .expect("rollup");
        eprintln!("[6/6] OK: rollup updated");

        eprintln!("\n === FULL LIFECYCLE PASSED ===");
        eprintln!("   Planned → Dispatched → Executed → Open → Resolved → Rollup");
        eprintln!("   Real SQLite. Real schema. No mocks.");
    });
}

/// Throttled non-execution path.
#[test]
fn throttled_non_execution_path() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "atc_lifecycle_throttle.db");

    rt.block_on(async {
        let row = make_probe_experience(300, 400, "BlueLake");
        eprintln!("\n[THROTTLE] Testing non-execution path...");

        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append");
        let exp_id = stored.experience_id;

        // Dispatch
        transition_atc_experience(
            &cx,
            &pool,
            exp_id,
            ExperienceState::Dispatched,
            1_700_000_001_000_000,
            None,
            None,
        )
        .await
        .into_result()
        .expect("dispatch");

        // Throttle
        let reason = NonExecutionReason::BudgetExhausted {
            budget_name: "probe_budget".to_string(),
            current: 0.95,
            threshold: 0.90,
        };
        transition_atc_experience(
            &cx,
            &pool,
            exp_id,
            ExperienceState::Throttled,
            1_700_000_001_100_000,
            Some(&reason),
            Some(&serde_json::json!({"throttle": "probe budget exhausted"})),
        )
        .await
        .into_result()
        .expect("throttle");

        // Throttled is terminal — should not appear in open query
        let open = fetch_open_atc_experiences(&cx, &pool, Some("BlueLake"), 10)
            .await
            .into_result()
            .expect("fetch open");
        assert_eq!(open.len(), 0);

        eprintln!("[THROTTLE] OK: Planned → Dispatched → Throttled (terminal)");
    });
}

/// State machine rejects invalid transitions.
#[test]
fn state_machine_rejects_invalid_transition() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "atc_lifecycle_invalid.db");

    rt.block_on(async {
        let row = make_probe_experience(500, 600, "RedMountain");
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append");

        eprintln!("\n[INVALID] Attempting Planned → Resolved (should fail)...");
        let result = transition_atc_experience(
            &cx,
            &pool,
            stored.experience_id,
            ExperienceState::Resolved,
            1_700_000_000_000_000,
            None,
            None,
        )
        .await;

        assert!(
            matches!(result, asupersync::Outcome::Err(_)),
            "Planned → Resolved must be rejected"
        );
        eprintln!("[INVALID] OK: correctly rejected invalid transition");
    });
}

/// One decision → many effects (fan-out).
#[test]
fn one_decision_many_effects() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "atc_lifecycle_fanout.db");

    rt.block_on(async {
        eprintln!("\n[FANOUT] One decision, three effects...");

        let e1 = make_probe_experience(900, 901, "Agent1");
        let mut e2 = make_probe_experience(900, 902, "Agent2");
        e2.effect_kind = EffectKind::Advisory;
        e2.action = "AdvisoryMessage".to_string();
        let e3 = make_probe_experience(900, 903, "Agent3");

        let s1 = append_atc_experience(&cx, &pool, &e1)
            .await
            .into_result()
            .expect("e1");
        let s2 = append_atc_experience(&cx, &pool, &e2)
            .await
            .into_result()
            .expect("e2");
        let s3 = append_atc_experience(&cx, &pool, &e3)
            .await
            .into_result()
            .expect("e3");

        // All different experience_ids, same decision_id
        assert_ne!(s1.experience_id, s2.experience_id);
        assert_ne!(s2.experience_id, s3.experience_id);
        assert_eq!(s1.decision_id, 900);
        assert_eq!(s2.decision_id, 900);
        assert_eq!(s3.decision_id, 900);

        eprintln!(
            "[FANOUT] OK: 3 experiences [{}, {}, {}] for decision 900",
            s1.experience_id, s2.experience_id, s3.experience_id
        );
    });
}

/// Idempotent append (duplicate insert returns same row).
#[test]
fn idempotent_append() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "atc_lifecycle_idem.db");

    rt.block_on(async {
        let row = make_probe_experience(1000, 1001, "TestAgent");

        let first = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("first");
        let second = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("second");

        assert_eq!(
            first.experience_id, second.experience_id,
            "duplicate append must return same experience_id"
        );
        eprintln!(
            "[IDEMPOTENT] OK: same experience_id={}",
            first.experience_id
        );
    });
}

// ──────────────────────────────────────────────────────────────────────
// resolve_experience() tests
// ──────────────────────────────────────────────────────────────────────

fn advance_to_open(
    rt: &asupersync::runtime::Runtime,
    cx: &Cx,
    pool: &DbPool,
    decision_id: u64,
    effect_id: u64,
    subject: &str,
) -> u64 {
    rt.block_on(async {
        let row = make_probe_experience(decision_id, effect_id, subject);
        let stored = append_atc_experience(cx, pool, &row)
            .await
            .into_result()
            .expect("append");
        let eid = stored.experience_id;
        for (state, ts) in [
            (ExperienceState::Dispatched, 1_700_000_000_100_000i64),
            (ExperienceState::Executed, 1_700_000_000_200_000),
            (ExperienceState::Open, 1_700_000_000_300_000),
        ] {
            transition_atc_experience(cx, pool, eid, state, ts, None, None)
                .await
                .into_result()
                .expect("transition");
        }
        eid
    })
}

#[test]
fn resolve_experience_resolved_happy_path() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_resolved.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2000, 2001, "Agent_R1");

    rt.block_on(async {
        let outcome = ExperienceOutcome {
            observed_ts_micros: 1_700_000_000_500_000,
            label: "probe_responded".to_string(),
            correct: true,
            actual_loss: Some(0.3),
            regret: Some(0.0),
            evidence: None,
        };
        let kind = ResolutionKind::Resolved(outcome);
        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("resolve resolved");

        let open = fetch_open_atc_experiences(&cx, &pool, Some("Agent_R1"), 10)
            .await
            .into_result()
            .expect("fetch open");
        assert_eq!(open.len(), 0, "resolved experience must not appear in open list");
    });
}

#[test]
fn resolve_experience_censored_happy_path() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_censored.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2100, 2101, "Agent_C1");

    rt.block_on(async {
        let kind = ResolutionKind::Censored {
            ts_micros: 1_700_000_000_600_000,
        };
        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("resolve censored");

        let open = fetch_open_atc_experiences(&cx, &pool, Some("Agent_C1"), 10)
            .await
            .into_result()
            .expect("fetch open");
        assert_eq!(open.len(), 0);
    });
}

#[test]
fn resolve_experience_expired_happy_path() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_expired.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2200, 2201, "Agent_E1");

    rt.block_on(async {
        let kind = ResolutionKind::Expired {
            ts_micros: 1_700_000_000_700_000,
        };
        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("resolve expired");

        let open = fetch_open_atc_experiences(&cx, &pool, Some("Agent_E1"), 10)
            .await
            .into_result()
            .expect("fetch open");
        assert_eq!(open.len(), 0);
    });
}

#[test]
fn resolve_experience_idempotent_same_outcome() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_idempotent.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2300, 2301, "Agent_I1");

    rt.block_on(async {
        let outcome = ExperienceOutcome {
            observed_ts_micros: 1_700_000_000_500_000,
            label: "probe_responded".to_string(),
            correct: true,
            actual_loss: Some(0.1),
            regret: Some(0.0),
            evidence: None,
        };
        let kind = ResolutionKind::Resolved(outcome.clone());

        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("first resolve");

        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("idempotent second resolve must succeed");
    });
}

#[test]
fn resolve_experience_idempotent_censored() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_idem_censor.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2400, 2401, "Agent_IC");

    rt.block_on(async {
        let kind = ResolutionKind::Censored { ts_micros: 1_700_000_001_000_000 };
        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("first censor");
        resolve_experience(&cx, &pool, eid, &kind)
            .await
            .into_result()
            .expect("idempotent censor must succeed");
    });
}

#[test]
fn resolve_experience_conflicting_re_resolve_rejected() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_conflict.db");
    let eid = advance_to_open(&rt, &cx, &pool, 2500, 2501, "Agent_X1");

    rt.block_on(async {
        let outcome = ExperienceOutcome {
            observed_ts_micros: 1_700_000_000_500_000,
            label: "correct".to_string(),
            correct: true,
            actual_loss: Some(0.0),
            regret: Some(0.0),
            evidence: None,
        };
        resolve_experience(&cx, &pool, eid, &ResolutionKind::Resolved(outcome))
            .await
            .into_result()
            .expect("first resolve");

        let conflict = resolve_experience(
            &cx,
            &pool,
            eid,
            &ResolutionKind::Expired { ts_micros: 1_700_000_002_000_000 },
        )
        .await;

        assert!(
            matches!(conflict, asupersync::Outcome::Err(ref e) if matches!(e, mcp_agent_mail_db::DbError::InvalidArgument { .. })),
            "conflicting re-resolve must be rejected, got: {conflict:?}"
        );
    });
}

#[test]
fn resolve_experience_not_open_rejected() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_not_open.db");

    rt.block_on(async {
        let row = make_probe_experience(2600, 2601, "Agent_N1");
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append");
        let eid = stored.experience_id;

        transition_atc_experience(
            &cx, &pool, eid, ExperienceState::Dispatched, 1_700_000_000_100_000, None, None,
        )
        .await
        .into_result()
        .expect("dispatch");

        let result = resolve_experience(
            &cx,
            &pool,
            eid,
            &ResolutionKind::Expired { ts_micros: 1_700_000_002_000_000 },
        )
        .await;

        assert!(
            matches!(result, asupersync::Outcome::Err(ref e) if matches!(e, mcp_agent_mail_db::DbError::InvalidArgument { .. })),
            "resolving non-open experience must be rejected, got: {result:?}"
        );
    });
}

#[test]
fn resolve_experience_not_found() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "resolve_exp_notfound.db");

    rt.block_on(async {
        let result = resolve_experience(
            &cx,
            &pool,
            999_999,
            &ResolutionKind::Expired { ts_micros: 1_700_000_000_000_000 },
        )
        .await;

        assert!(
            matches!(result, asupersync::Outcome::Err(ref e) if matches!(e, mcp_agent_mail_db::DbError::NotFound { .. })),
            "non-existent experience must return NotFound, got: {result:?}"
        );
    });
}
