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
    FeatureVector, MAX_ATC_CONTENT_BYTES, MAX_ATC_CONTENT_JSON_DEPTH, MAX_ATC_CONTENT_NODES,
    MAX_ATC_CONTENT_STRING_BYTES, NonExecutionReason, ResolutionKind,
};
use mcp_agent_mail_db::queries::{
    append_atc_experience, fetch_open_atc_experiences, overwrite_resolved_atc_experience_outcome,
    record_atc_execution_outcome, resolve_atc_experience, resolve_atc_observed_outcome,
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
        // state=resolved and outcome_json is populated. File-backed ATC rows
        // live in the canonical atc.sqlite3 sidecar (br-bvq1x.11.7), so the
        // verification read goes there, not to the Franken main DB.
        let db_path = _dir.path().join("atc_lifecycle_full.db");
        let verify_conn = mcp_agent_mail_db::CanonicalDbConn::open_file(
            mcp_agent_mail_db::pool::atc_sidecar_sqlite_path(&db_path.display().to_string()),
        )
        .expect("open ATC sidecar for verification");
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
        assert_eq!(
            open.len(),
            0,
            "resolved experience must not appear in open list"
        );
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
        let kind = ResolutionKind::Censored {
            ts_micros: 1_700_000_001_000_000,
        };
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

// br-kp1in.50: the public persistence entrypoints, including post-insert
// amendments, must enforce content policy without relying on insert_experience.
// These are synthetic credentials, never values from a mailbox or environment.
const PRIVACY_CANARY: &str = "Bearer atc_synthetic_canary_never_a_real_secret_0123456789";
const PRIVACY_CANARY_MARKER: &str = "atc_synthetic_canary";

#[derive(Clone, Copy)]
enum PrivacyStorage {
    File,
    Memory,
}

fn setup_privacy_db(
    rt: &asupersync::runtime::Runtime,
    storage: PrivacyStorage,
) -> (Cx, DbPool, Option<tempfile::TempDir>) {
    match storage {
        PrivacyStorage::File => {
            let (cx, pool, dir) = setup_real_db(rt, "atc_content_boundary.db");
            (cx, pool, Some(dir))
        }
        PrivacyStorage::Memory => {
            let cfg = DbPoolConfig {
                database_url: "sqlite:///:memory:".to_string(),
                min_connections: 1,
                max_connections: 1,
                warmup_connections: 0,
                ..Default::default()
            };
            (
                Cx::for_testing(),
                create_pool(&cfg).expect("create real in-memory pool"),
                None,
            )
        }
    }
}

async fn privacy_query_rows(cx: &Cx, pool: &DbPool, sql: &str) -> Vec<sqlmodel_core::Row> {
    if pool.sqlite_path() == ":memory:" {
        let conn = pool
            .acquire(cx)
            .await
            .into_result()
            .expect("acquire memory DB");
        conn.query_sync(sql, &[]).expect("inspect real memory DB")
    } else {
        // Every verification gets a fresh, independent sidecar handle. A cached
        // returned row or an error alone cannot establish durable non-mutation.
        let path = mcp_agent_mail_db::pool::atc_sidecar_sqlite_path(pool.sqlite_path());
        let conn = mcp_agent_mail_db::CanonicalDbConn::open_file(path)
            .expect("independently reopen ATC sidecar");
        conn.query_sync(sql, &[])
            .expect("inspect reopened ATC sidecar")
    }
}

async fn privacy_snapshot(cx: &Cx, pool: &DbPool) -> String {
    let rows = privacy_query_rows(
        cx,
        pool,
        "SELECT * FROM atc_experiences ORDER BY experience_id",
    )
    .await;
    let rollups = privacy_query_rows(
        cx,
        pool,
        "SELECT * FROM atc_experience_rollups ORDER BY stratum_key",
    )
    .await;
    format!("{rows:?}\n{rollups:?}")
}

async fn assert_privacy_snapshot_unchanged(cx: &Cx, pool: &DbPool, before: &str) {
    let after = privacy_snapshot(cx, pool).await;
    // Avoid printing database content on failure, even though fixtures contain
    // only synthetic data. This also verifies timestamps and every rollup field.
    assert!(
        before == after,
        "rejected mutation changed stored rows or rollups"
    );
    assert!(
        !after.contains(PRIVACY_CANARY_MARKER),
        "canary reached storage"
    );
}

fn assert_content_rejected<T>(
    result: asupersync::Outcome<T, mcp_agent_mail_db::DbError>,
    api: &str,
) {
    let asupersync::Outcome::Err(error) = result else {
        panic!("api={api}: expected a content-validation error");
    };
    assert!(
        matches!(&error, mcp_agent_mail_db::DbError::InvalidArgument { .. }),
        "api={api}: rejection must be a validation error"
    );
    let diagnostic = format!("{error} {error:?}");
    assert!(
        !diagnostic.contains(PRIVACY_CANARY_MARKER),
        "api={api}: rejection diagnostic echoed synthetic content"
    );
    eprintln!("[ATC-PRIVACY] api={api} result=rejected category=content");
}

async fn seed_privacy_rollup(cx: &Cx, pool: &DbPool) {
    update_atc_experience_rollup(
        cx,
        pool,
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
    .expect("seed nonempty rollup witness");
}

fn privacy_outcome() -> ExperienceOutcome {
    ExperienceOutcome {
        observed_ts_micros: 1_700_000_000_500_000,
        label: "agent_responded_within_window".to_string(),
        correct: true,
        actual_loss: Some(0.5),
        regret: Some(0.0),
        evidence: Some(serde_json::json!({
            "response_ms": 1800,
            "token_count": 12,
            "status": "token_bucket",
            "agent": "GreenCastle",
            "labels": ["task-complete", "risk-low"],
            "optional": null,
            "healthy": true
        })),
    }
}

fn check_direct_append_content_boundaries(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    rt.block_on(async {
        let valid = make_probe_experience(3000, 3001, "GreenCastle");
        append_atc_experience(&cx, &pool, &valid)
            .await
            .into_result()
            .expect("append clean baseline");
        seed_privacy_rollup(&cx, &pool).await;
        let before = privacy_snapshot(&cx, &pool).await;

        // Each otherwise-valid row plants the canary in exactly one field.
        let mut resolved = make_probe_experience(3010, 3011, "BlueLake");
        resolved.state = ExperienceState::Resolved;
        resolved.dispatched_ts_micros = Some(1_700_000_000_100_000);
        resolved.executed_ts_micros = Some(1_700_000_000_200_000);
        resolved.resolved_ts_micros = Some(1_700_000_000_500_000);
        resolved.outcome = Some(privacy_outcome());
        let mut candidates = Vec::new();
        let mut row = resolved.clone();
        row.evidence_summary = PRIVACY_CANARY.to_string();
        candidates.push(("evidence_summary", row));
        let mut row = resolved.clone();
        row.context = Some(serde_json::json!({"metrics": [{"value": PRIVACY_CANARY}]}));
        candidates.push(("context_value", row));
        let mut row = resolved.clone();
        row.context = Some(serde_json::json!({(PRIVACY_CANARY): 1}));
        candidates.push(("context_key", row));
        let mut row = resolved.clone();
        row.outcome.as_mut().expect("outcome").label = PRIVACY_CANARY.to_string();
        candidates.push(("outcome_label", row));
        let mut row = resolved.clone();
        row.outcome.as_mut().expect("outcome").evidence =
            Some(serde_json::json!({"observations": [{"nested": [PRIVACY_CANARY]}]}));
        candidates.push(("nested_outcome_evidence", row));
        let mut row = resolved;
        row.outcome.as_mut().expect("outcome").evidence =
            Some(serde_json::json!({"observations": [{(PRIVACY_CANARY): false}]}));
        candidates.push(("outcome_evidence_key", row));

        for (index, (field, mut row)) in candidates.into_iter().enumerate() {
            row.decision_id += u64::try_from(index).expect("small fixture index");
            row.effect_id += u64::try_from(index).expect("small fixture index");
            assert_content_rejected(
                append_atc_experience(&cx, &pool, &row).await,
                "append_atc_experience",
            );
            assert_privacy_snapshot_unchanged(&cx, &pool, &before).await;
            eprintln!("[ATC-PRIVACY] api=append_atc_experience field={field} stored=unchanged");
        }

        let mut replay = valid;
        replay.evidence_summary = PRIVACY_CANARY.to_string();
        assert_content_rejected(
            append_atc_experience(&cx, &pool, &replay).await,
            "append_atc_experience_replay",
        );
        assert_privacy_snapshot_unchanged(&cx, &pool, &before).await;
    });
}

#[test]
fn direct_append_rejects_content_after_sidecar_reopen() {
    check_direct_append_content_boundaries(PrivacyStorage::File);
}

#[test]
fn direct_append_rejects_content_in_memory() {
    check_direct_append_content_boundaries(PrivacyStorage::Memory);
}

async fn reject_context_transition(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    state: ExperienceState,
    reason: Option<&NonExecutionReason>,
    context: Option<&serde_json::Value>,
) {
    let before = privacy_snapshot(cx, pool).await;
    assert_content_rejected(
        transition_atc_experience(
            cx,
            pool,
            experience_id,
            state,
            1_700_000_000_400_000,
            reason,
            context,
        )
        .await,
        "transition_atc_experience",
    );
    assert_privacy_snapshot_unchanged(cx, pool, &before).await;
    eprintln!("[ATC-PRIVACY] api=transition_atc_experience target_state={state} stored=unchanged");
}

#[derive(Clone, Copy)]
enum PrivacyOutcomeWrite {
    Resolve,
    Unified,
    Amend,
    Observed,
}

async fn reject_outcome_write(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    operation: PrivacyOutcomeWrite,
    outcome: &ExperienceOutcome,
) {
    let before = privacy_snapshot(cx, pool).await;
    let (api, result) = match operation {
        PrivacyOutcomeWrite::Resolve => (
            "resolve_atc_experience",
            resolve_atc_experience(cx, pool, experience_id, outcome).await,
        ),
        PrivacyOutcomeWrite::Unified => (
            "resolve_experience",
            resolve_experience(
                cx,
                pool,
                experience_id,
                &ResolutionKind::Resolved(outcome.clone()),
            )
            .await,
        ),
        PrivacyOutcomeWrite::Amend => (
            "overwrite_resolved_atc_experience_outcome",
            overwrite_resolved_atc_experience_outcome(cx, pool, experience_id, outcome).await,
        ),
        PrivacyOutcomeWrite::Observed => (
            "resolve_atc_observed_outcome",
            resolve_atc_observed_outcome(cx, pool, experience_id, outcome).await,
        ),
    };
    assert_content_rejected(result, api);
    assert_privacy_snapshot_unchanged(cx, pool, &before).await;
}

fn check_post_insert_content_boundaries(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    rt.block_on(async {
        let stored = append_atc_experience(
            &cx,
            &pool,
            &make_probe_experience(4000, 4001, "GreenCastle"),
        )
        .await
        .into_result()
        .expect("append clean baseline");
        seed_privacy_rollup(&cx, &pool).await;
        let id = stored.experience_id;
        let bad_context = serde_json::json!({"execution": [{"detail": PRIVACY_CANARY}]});
        for target in [ExperienceState::Planned, ExperienceState::Dispatched] {
            reject_context_transition(&cx, &pool, id, target, None, Some(&bad_context)).await;
        }
        transition_atc_experience(
            &cx,
            &pool,
            id,
            ExperienceState::Dispatched,
            1_700_000_000_100_000,
            None,
            None,
        )
        .await
        .into_result()
        .expect("valid dispatch after refusals");
        reject_context_transition(
            &cx,
            &pool,
            id,
            ExperienceState::Executed,
            None,
            Some(&bad_context),
        )
        .await;

        for (state, reason) in [
            (
                ExperienceState::Throttled,
                NonExecutionReason::BudgetExhausted {
                    budget_name: PRIVACY_CANARY.to_string(),
                    current: 1.0,
                    threshold: 0.9,
                },
            ),
            (
                ExperienceState::Suppressed,
                NonExecutionReason::SafetyGate {
                    gate_name: PRIVACY_CANARY.to_string(),
                    risk_score: 0.9,
                    gate_threshold: 0.8,
                },
            ),
            (
                ExperienceState::Skipped,
                NonExecutionReason::CalibrationFallback {
                    reason: PRIVACY_CANARY.to_string(),
                },
            ),
        ] {
            reject_context_transition(&cx, &pool, id, state, Some(&reason), None).await;
        }
        for state in [ExperienceState::Executed, ExperienceState::Open] {
            transition_atc_experience(&cx, &pool, id, state, 1_700_000_000_300_000, None, None)
                .await
                .into_result()
                .expect("valid lifecycle after rejected execution metadata");
        }
        reject_context_transition(
            &cx,
            &pool,
            id,
            ExperienceState::Open,
            None,
            Some(&bad_context),
        )
        .await;

        let mut label_canary = privacy_outcome();
        label_canary.label = PRIVACY_CANARY.to_string();
        let mut evidence_canary = privacy_outcome();
        evidence_canary.evidence = Some(serde_json::json!({"observations": [[PRIVACY_CANARY]]}));
        let mut key_canary = privacy_outcome();
        key_canary.evidence = Some(serde_json::json!({"nested": {(PRIVACY_CANARY): 0}}));
        for operation in [
            PrivacyOutcomeWrite::Resolve,
            PrivacyOutcomeWrite::Unified,
            PrivacyOutcomeWrite::Observed,
        ] {
            for outcome in [&label_canary, &evidence_canary, &key_canary] {
                reject_outcome_write(&cx, &pool, id, operation, outcome).await;
            }
        }

        resolve_atc_experience(&cx, &pool, id, &privacy_outcome())
            .await
            .into_result()
            .expect("valid outcome after refusals");
        for outcome in [&label_canary, &evidence_canary, &key_canary] {
            reject_outcome_write(&cx, &pool, id, PrivacyOutcomeWrite::Amend, outcome).await;
        }
        reject_context_transition(
            &cx,
            &pool,
            id,
            ExperienceState::Resolved,
            None,
            Some(&bad_context),
        )
        .await;
        let mut acknowledged = privacy_outcome();
        acknowledged.label = "acknowledged".to_string();
        overwrite_resolved_atc_experience_outcome(&cx, &pool, id, &acknowledged)
            .await
            .into_result()
            .expect("valid ack-over-read amendment after refusals");
        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, outcome_json FROM atc_experiences",
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "resolved"
        );
        let outcome: ExperienceOutcome = serde_json::from_str(
            &rows[0]
                .get_named::<String>("outcome_json")
                .expect("stored outcome"),
        )
        .expect("outcome JSON");
        assert_eq!(outcome, acknowledged);
    });
}

#[test]
fn post_insert_updates_reject_content_after_sidecar_reopen() {
    check_post_insert_content_boundaries(PrivacyStorage::File);
}

#[test]
fn post_insert_updates_reject_content_in_memory() {
    check_post_insert_content_boundaries(PrivacyStorage::Memory);
}

fn check_content_resource_bounds(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    let id = advance_to_open(&rt, &cx, &pool, 5000, 5001, "GreenCastle");
    rt.block_on(async {
        seed_privacy_rollup(&cx, &pool).await;
        let before = privacy_snapshot(&cx, &pool).await;
        let mut deep = serde_json::Value::Null;
        for _ in 0..=MAX_ATC_CONTENT_JSON_DEPTH {
            deep = serde_json::json!([deep]);
        }
        let payloads = [
            ("depth", serde_json::json!({"samples": deep})),
            (
                "work",
                serde_json::json!({"samples": vec![0; MAX_ATC_CONTENT_NODES + 1]}),
            ),
            (
                "bytes",
                serde_json::json!({"samples": vec![
                    "x".repeat(MAX_ATC_CONTENT_STRING_BYTES);
                    MAX_ATC_CONTENT_BYTES / MAX_ATC_CONTENT_STRING_BYTES + 1
                ]}),
            ),
            (
                "string",
                serde_json::json!({"status": "x".repeat(MAX_ATC_CONTENT_STRING_BYTES + 1)}),
            ),
        ];
        for (limit, payload) in payloads {
            let mut row = make_probe_experience(5010, 5011, "BlueLake");
            row.context = Some(payload.clone());
            assert_content_rejected(
                append_atc_experience(&cx, &pool, &row).await,
                "append_atc_experience_resource_bound",
            );
            assert_privacy_snapshot_unchanged(&cx, &pool, &before).await;
            reject_context_transition(&cx, &pool, id, ExperienceState::Open, None, Some(&payload))
                .await;
            let mut outcome = privacy_outcome();
            outcome.evidence = Some(payload);
            reject_outcome_write(&cx, &pool, id, PrivacyOutcomeWrite::Resolve, &outcome).await;
            eprintln!("[ATC-PRIVACY] resource_limit={limit} stored=unchanged");
        }

        // Independently small patches must not grow stored context past the
        // shared budget. Checking only incoming patches misses this boundary.
        let mut initial = serde_json::Map::new();
        let mut addition = serde_json::Map::new();
        for index in 0..12 {
            initial.insert(
                format!("initial_{index}"),
                serde_json::json!("x".repeat(3000)),
            );
            addition.insert(
                format!("added_{index}"),
                serde_json::json!("x".repeat(3000)),
            );
        }
        let mut row = make_probe_experience(5020, 5021, "BlueLake");
        row.context = Some(serde_json::Value::Object(initial));
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("bounded initial context");
        reject_context_transition(
            &cx,
            &pool,
            stored.experience_id,
            ExperienceState::Dispatched,
            None,
            Some(&serde_json::Value::Object(addition)),
        )
        .await;

        // The persisted context and an incoming outcome must share one row
        // budget. Each payload below passes by itself, but their sum does not.
        let mut aggregate_overflow = privacy_outcome();
        aggregate_overflow.evidence = row.context.clone();
        assert!(aggregate_overflow.validate_content().is_ok());
        for state in [
            ExperienceState::Dispatched,
            ExperienceState::Executed,
            ExperienceState::Open,
        ] {
            transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                state,
                1_700_000_000_300_000,
                None,
                None,
            )
            .await
            .into_result()
            .expect("advance bounded context to open");
        }
        for operation in [
            PrivacyOutcomeWrite::Resolve,
            PrivacyOutcomeWrite::Unified,
            PrivacyOutcomeWrite::Observed,
        ] {
            reject_outcome_write(
                &cx,
                &pool,
                stored.experience_id,
                operation,
                &aggregate_overflow,
            )
            .await;
        }
        resolve_atc_experience(&cx, &pool, stored.experience_id, &privacy_outcome())
            .await
            .into_result()
            .expect("small outcome still fits the row budget");
        reject_outcome_write(
            &cx,
            &pool,
            stored.experience_id,
            PrivacyOutcomeWrite::Amend,
            &aggregate_overflow,
        )
        .await;
    });
}

#[test]
fn content_limits_preserve_rows_and_rollups_after_sidecar_reopen() {
    check_content_resource_bounds(PrivacyStorage::File);
}

#[test]
fn content_limits_preserve_rows_and_rollups_in_memory() {
    check_content_resource_bounds(PrivacyStorage::Memory);
}

fn check_execution_record_is_atomic(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    rt.block_on(async {
        let mut initial = serde_json::Map::new();
        let mut addition = serde_json::Map::new();
        for index in 0..12 {
            initial.insert(
                format!("initial_{index}"),
                serde_json::json!("x".repeat(3000)),
            );
            addition.insert(
                format!("added_{index}"),
                serde_json::json!("x".repeat(3000)),
            );
        }
        let mut row = make_probe_experience(5500, 5501, "GreenCastle");
        row.context = Some(serde_json::Value::Object(initial));
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append planned execution with bounded context");
        seed_privacy_rollup(&cx, &pool).await;
        let before = privacy_snapshot(&cx, &pool).await;
        let bad_context = serde_json::json!({"execution": {"detail": PRIVACY_CANARY}});
        let oversized_merged_context = serde_json::Value::Object(addition);
        for context in [&bad_context, &oversized_merged_context] {
            assert_content_rejected(
                record_atc_execution_outcome(
                    &cx,
                    &pool,
                    stored.experience_id,
                    ExperienceState::Executed,
                    1_700_000_000_200_000,
                    None,
                    Some(context),
                )
                .await,
                "record_atc_execution_outcome",
            );
            assert_privacy_snapshot_unchanged(&cx, &pool, &before).await;
        }
        for (state, reason) in [
            (
                ExperienceState::Suppressed,
                NonExecutionReason::SafetyGate {
                    gate_name: PRIVACY_CANARY.to_string(),
                    risk_score: 0.9,
                    gate_threshold: 0.8,
                },
            ),
            (
                ExperienceState::Skipped,
                NonExecutionReason::CalibrationFallback {
                    reason: PRIVACY_CANARY.to_string(),
                },
            ),
        ] {
            assert_content_rejected(
                record_atc_execution_outcome(
                    &cx,
                    &pool,
                    stored.experience_id,
                    state,
                    1_700_000_000_200_000,
                    Some(&reason),
                    None,
                )
                .await,
                "record_atc_execution_outcome_nonexecution",
            );
            assert_privacy_snapshot_unchanged(&cx, &pool, &before).await;
        }
        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, dispatched_ts, executed_ts FROM atc_experiences",
        )
        .await;
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "planned"
        );
        assert!(matches!(rows[0].get(1), Some(sqlmodel_core::Value::Null)));
        assert!(matches!(rows[0].get(2), Some(sqlmodel_core::Value::Null)));

        record_atc_execution_outcome(
            &cx,
            &pool,
            stored.experience_id,
            ExperienceState::Executed,
            1_700_000_000_200_000,
            None,
            Some(&serde_json::json!({"attempt_count": 1, "status": "task-complete"})),
        )
        .await
        .into_result()
        .expect("accept complete execution atomically after refusals");
        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, dispatched_ts, executed_ts FROM atc_experiences",
        )
        .await;
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "executed"
        );
        assert_eq!(
            rows[0]
                .get_named::<i64>("dispatched_ts")
                .expect("dispatch time"),
            1_700_000_000_200_000
        );
        assert_eq!(
            rows[0]
                .get_named::<i64>("executed_ts")
                .expect("execution time"),
            1_700_000_000_200_000
        );
    });
}

#[test]
fn execution_record_rejection_is_atomic_after_sidecar_reopen() {
    check_execution_record_is_atomic(PrivacyStorage::File);
}

#[test]
fn execution_record_rejection_is_atomic_in_memory() {
    check_execution_record_is_atomic(PrivacyStorage::Memory);
}

fn check_observed_outcome_is_atomic(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    rt.block_on(async {
        let mut context = serde_json::Map::new();
        for index in 0..12 {
            context.insert(
                format!("metric_{index}"),
                serde_json::json!("x".repeat(3000)),
            );
        }
        let mut row = make_probe_experience(5600, 5601, "GreenCastle");
        row.context = Some(serde_json::Value::Object(context));
        let stored = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("append planned observation with bounded context");
        seed_privacy_rollup(&cx, &pool).await;
        let id = stored.experience_id;
        let before_planned_refusal = privacy_snapshot(&cx, &pool).await;
        let premature = resolve_atc_observed_outcome(&cx, &pool, id, &privacy_outcome()).await;
        assert!(
            matches!(
                premature,
                asupersync::Outcome::Err(mcp_agent_mail_db::DbError::InvalidArgument { .. })
            ),
            "a valid outcome must not bypass the planned lifecycle state"
        );
        assert_privacy_snapshot_unchanged(&cx, &pool, &before_planned_refusal).await;

        record_atc_execution_outcome(
            &cx,
            &pool,
            id,
            ExperienceState::Executed,
            1_700_000_000_200_000,
            None,
            None,
        )
        .await
        .into_result()
        .expect("record real executed observation");
        let mut label_canary = privacy_outcome();
        label_canary.label = PRIVACY_CANARY.to_string();
        let mut evidence_canary = privacy_outcome();
        evidence_canary.evidence = Some(serde_json::json!({"observations": [[PRIVACY_CANARY]]}));
        let mut key_canary = privacy_outcome();
        key_canary.evidence = Some(serde_json::json!({"nested": {(PRIVACY_CANARY): 0}}));
        let mut aggregate_overflow = privacy_outcome();
        aggregate_overflow.evidence = row.context.clone();
        assert!(aggregate_overflow.validate_content().is_ok());
        for outcome in [
            &label_canary,
            &evidence_canary,
            &key_canary,
            &aggregate_overflow,
        ] {
            reject_outcome_write(&cx, &pool, id, PrivacyOutcomeWrite::Observed, outcome).await;
        }
        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, resolved_ts, outcome_json FROM atc_experiences",
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "executed"
        );
        assert!(matches!(rows[0].get(1), Some(sqlmodel_core::Value::Null)));
        assert!(matches!(rows[0].get(2), Some(sqlmodel_core::Value::Null)));

        let outcome = privacy_outcome();
        resolve_atc_observed_outcome(&cx, &pool, id, &outcome)
            .await
            .into_result()
            .expect("valid observed outcome advances executed to resolved atomically");
        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, resolved_ts, outcome_json FROM atc_experiences",
        )
        .await;
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "resolved"
        );
        assert_eq!(
            rows[0]
                .get_named::<i64>("resolved_ts")
                .expect("resolution timestamp"),
            outcome.observed_ts_micros
        );
        let persisted_outcome: ExperienceOutcome = serde_json::from_str(
            &rows[0]
                .get_named::<String>("outcome_json")
                .expect("outcome"),
        )
        .expect("stored outcome JSON");
        assert_eq!(persisted_outcome, outcome);
        let before_replay = privacy_snapshot(&cx, &pool).await;
        let mut replay = outcome;
        replay.label = "acknowledged".to_string();
        replay.observed_ts_micros += 1_000;
        resolve_atc_observed_outcome(&cx, &pool, id, &replay)
            .await
            .into_result()
            .expect("valid already-resolved observed outcome is a no-op");
        assert_privacy_snapshot_unchanged(&cx, &pool, &before_replay).await;
    });
}

#[test]
fn observed_outcome_rejection_is_atomic_after_sidecar_reopen() {
    check_observed_outcome_is_atomic(PrivacyStorage::File);
}

#[test]
fn observed_outcome_rejection_is_atomic_in_memory() {
    check_observed_outcome_is_atomic(PrivacyStorage::Memory);
}

fn check_valid_metrics_and_idempotency(storage: PrivacyStorage) {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_privacy_db(&rt, storage);
    rt.block_on(async {
        let mut row = make_probe_experience(6000, 6001, "GreenCastle");
        row.context = Some(serde_json::json!({
            "message_id": 41,
            "recipient_count": 2,
            "ack_required": true,
            "message_bytes": 2048,
            "token_count": 12,
            "status": "token_bucket"
        }));
        let first = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("accept ordinary message metrics");
        let before_replay = privacy_snapshot(&cx, &pool).await;
        row.evidence_summary = "reservation_count=3 conflict_count=0".to_string();
        row.context = Some(serde_json::json!({"reservation_count": 3, "conflict_count": 0}));
        let replay = append_atc_experience(&cx, &pool, &row)
            .await
            .into_result()
            .expect("valid duplicate append");
        assert_eq!(
            first, replay,
            "first insert must win even if a valid retry differs"
        );
        assert_privacy_snapshot_unchanged(&cx, &pool, &before_replay).await;

        for state in [
            ExperienceState::Dispatched,
            ExperienceState::Executed,
            ExperienceState::Open,
        ] {
            transition_atc_experience(
                &cx,
                &pool,
                first.experience_id,
                state,
                1_700_000_000_300_000,
                None,
                Some(&serde_json::json!({"latency_ms": 1800, "status": "task-complete"})),
            )
            .await
            .into_result()
            .expect("accept ordinary execution metadata");
        }
        let outcome = privacy_outcome();
        resolve_experience(
            &cx,
            &pool,
            first.experience_id,
            &ResolutionKind::Resolved(outcome.clone()),
        )
        .await
        .into_result()
        .expect("accept ordinary outcome metrics");
        seed_privacy_rollup(&cx, &pool).await;
        let before_resolution_replay = privacy_snapshot(&cx, &pool).await;
        resolve_experience(
            &cx,
            &pool,
            first.experience_id,
            &ResolutionKind::Resolved(outcome.clone()),
        )
        .await
        .into_result()
        .expect("idempotent outcome resolution");
        assert_privacy_snapshot_unchanged(&cx, &pool, &before_resolution_replay).await;

        let rows = privacy_query_rows(
            &cx,
            &pool,
            "SELECT state, context_json, outcome_json FROM atc_experiences",
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("state").expect("state"),
            "resolved"
        );
        let context: serde_json::Value = serde_json::from_str(
            &rows[0]
                .get_named::<String>("context_json")
                .expect("context"),
        )
        .expect("stored context JSON");
        assert_eq!(context["message_id"], 41);
        assert_eq!(context["token_count"], 12);
        assert_eq!(context["latency_ms"], 1800);
        let persisted_outcome: ExperienceOutcome = serde_json::from_str(
            &rows[0]
                .get_named::<String>("outcome_json")
                .expect("outcome"),
        )
        .expect("stored outcome JSON");
        assert_eq!(persisted_outcome, outcome);
        let rollups = privacy_query_rows(
            &cx,
            &pool,
            "SELECT total_count, resolved_count, correct_count, total_loss, total_regret \
             FROM atc_experience_rollups",
        )
        .await;
        assert_eq!(rollups.len(), 1);
        assert_eq!(
            rollups[0].get_named::<i64>("total_count").expect("total"),
            1
        );
        assert_eq!(
            rollups[0]
                .get_named::<i64>("resolved_count")
                .expect("resolved"),
            1
        );
        assert_eq!(
            rollups[0]
                .get_named::<i64>("correct_count")
                .expect("correct"),
            1
        );
        assert!(
            (rollups[0].get_named::<f64>("total_loss").expect("loss") - 0.5).abs() < f64::EPSILON
        );
        assert!(
            rollups[0]
                .get_named::<f64>("total_regret")
                .expect("regret")
                .abs()
                < f64::EPSILON
        );
    });
}

#[test]
fn valid_metrics_lifecycle_and_idempotency_survive_sidecar_reopen() {
    check_valid_metrics_and_idempotency(PrivacyStorage::File);
}

#[test]
fn valid_metrics_lifecycle_and_idempotency_work_in_memory() {
    check_valid_metrics_and_idempotency(PrivacyStorage::Memory);
}
