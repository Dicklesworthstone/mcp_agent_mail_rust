#![recursion_limit = "256"]

//! GH#213: a no-name `register_agent` must NEVER mutate an existing agent.
//!
//! The 2026-08-15 reporter measured 4/187 (Linux) and 12/507 (Windows) acked
//! auto-named registrations silently merging onto already-registered agents:
//! the random adjective+noun draw collided with an existing name and the
//! shared `INSERT .. ON CONFLICT(project_id, name) DO UPDATE` upsert replaced
//! that agent's `program` and `task_description` while acking the caller as a
//! fresh registration.
//!
//! These tests pin the fixed semantics:
//! - auto-name collision redraws and leaves the existing agent untouched;
//! - redraw exhaustion surfaces a clear CONFLICT error, never a merge;
//! - explicit-name re-registration keeps its documented upsert semantics.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    claim_fresh_auto_named_agent, ensure_project, register_agent, tool_util::get_db_pool,
};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
    time_component.wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_serial_async<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_suffix = unique_suffix();
    let db_path = format!("/tmp/auto-name-collision-{env_suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/auto-name-collision-storage-{env_suffix}");
    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database_url.as_str()),
            ("STORAGE_ROOT", storage_root.as_str()),
        ],
        || {
            Config::reset_cached();
            let cx = Cx::for_testing();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            rt.block_on(f(cx))
        },
    )
}

fn parse_json(payload: &str) -> Value {
    serde_json::from_str(payload).expect("tool response must be valid JSON")
}

async fn register_explicit(
    ctx: &McpContext,
    project_key: &str,
    name: &str,
    program: &str,
    task: &str,
) -> Value {
    let payload = register_agent(
        ctx,
        project_key.to_string(),
        program.to_string(),
        "opus-4.5".to_string(),
        Some(name.to_string()),
        Some(task.to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("explicit register_agent should succeed");
    parse_json(&payload)
}

/// (a) Auto-name collision must redraw: the existing agent keeps its id,
/// program, and `task_description`; the new registration lands under the
/// redrawn name.
#[test]
fn auto_name_collision_redraws_without_touching_existing_agent() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/auto-name-collision-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);

        let project = parse_json(
            &ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project"),
        );
        let project_id = project["id"].as_i64().expect("project id");

        let core = register_explicit(
            &ctx,
            &project_key,
            "BlueLake",
            "wsl2-trial-core",
            "core work",
        )
        .await;
        let core_id = core["id"].as_i64().expect("core agent id");

        let pool = get_db_pool().expect("db pool");
        // Deterministic drawer: first candidate collides with the existing
        // agent; the redraw yields a fresh name.
        let row = claim_fresh_auto_named_agent(
            &ctx,
            &pool,
            &project_key,
            project_id,
            "wsl2-trial-fleet",
            "opus-4.5",
            Some("phase1 R2-25"),
            "auto",
            None,
            "BlueLake".to_string(),
            || "RedStone".to_string(),
        )
        .await
        .expect("collision must redraw, not fail or merge");

        assert_eq!(row.name, "RedStone", "redraw must claim the fresh name");
        assert_eq!(row.program, "wsl2-trial-fleet");
        assert_eq!(row.task_description, "phase1 R2-25");
        assert_ne!(
            row.id,
            Some(core_id),
            "auto-named registration must create a NEW row, never reuse the collided agent's id"
        );

        // The collided agent's identity fields must be untouched.
        let existing =
            match mcp_agent_mail_db::queries::get_agent(&cx, &pool, project_id, "BlueLake").await {
                asupersync::Outcome::Ok(agent) => agent,
                other => panic!("get_agent(BlueLake) failed: {other:?}"),
            };
        assert_eq!(existing.id, Some(core_id));
        assert_eq!(
            existing.program, "wsl2-trial-core",
            "existing agent's program must not be overwritten by a colliding auto-name draw"
        );
        assert_eq!(
            existing.task_description, "core work",
            "existing agent's task_description must not be overwritten by a colliding auto-name draw"
        );
    });
}

/// Redraw exhaustion (every draw collides) must surface a clear CONFLICT
/// error — and still never mutate the existing agent.
#[test]
fn auto_name_exhaustion_errors_instead_of_merging() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/auto-name-exhaustion-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);

        let project = parse_json(
            &ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project"),
        );
        let project_id = project["id"].as_i64().expect("project id");

        let core = register_explicit(
            &ctx,
            &project_key,
            "BlueLake",
            "wsl2-trial-core",
            "core work",
        )
        .await;
        let core_id = core["id"].as_i64().expect("core agent id");

        let pool = get_db_pool().expect("db pool");
        let err = claim_fresh_auto_named_agent(
            &ctx,
            &pool,
            &project_key,
            project_id,
            "wsl2-trial-fleet",
            "opus-4.5",
            Some("phase1 R2-25"),
            "auto",
            None,
            "BlueLake".to_string(),
            || "BlueLake".to_string(),
        )
        .await
        .expect_err("exhausted namespace must error, never merge");

        let error_type = err
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("error"))
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        assert_eq!(error_type, "CONFLICT", "exhaustion error code");
        assert!(
            err.message.contains("auto-generate"),
            "message should explain the auto-name exhaustion: {}",
            err.message
        );

        let existing =
            match mcp_agent_mail_db::queries::get_agent(&cx, &pool, project_id, "BlueLake").await {
                asupersync::Outcome::Ok(agent) => agent,
                other => panic!("get_agent(BlueLake) failed: {other:?}"),
            };
        assert_eq!(existing.id, Some(core_id));
        assert_eq!(existing.program, "wsl2-trial-core");
        assert_eq!(existing.task_description, "core work");
    });
}

/// (b) Explicit-name re-registration keeps its documented upsert semantics:
/// same row id, refreshed program/task.
#[test]
fn explicit_name_reregistration_still_upserts() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/explicit-upsert-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);

        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        let first = register_explicit(&ctx, &project_key, "GoldHawk", "program-a", "first").await;
        let first_id = first["id"].as_i64().expect("agent id");

        let second = register_explicit(&ctx, &project_key, "GoldHawk", "program-b", "second").await;
        assert_eq!(
            second["id"].as_i64(),
            Some(first_id),
            "explicit re-registration must update the same row"
        );
        assert_eq!(second["name"].as_str(), Some("GoldHawk"));
        assert_eq!(second["program"].as_str(), Some("program-b"));
        assert_eq!(second["task_description"].as_str(), Some("second"));
    });
}

/// End-to-end through the real tool: a no-name `register_agent` always
/// produces a NEW agent row and leaves every pre-registered agent intact.
#[test]
fn register_agent_without_name_never_reuses_existing_row() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/no-name-e2e-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);

        let project = parse_json(
            &ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project"),
        );
        let project_id = project["id"].as_i64().expect("project id");

        let core = register_explicit(
            &ctx,
            &project_key,
            "BlueLake",
            "wsl2-trial-core",
            "core work",
        )
        .await;
        let core_id = core["id"].as_i64().expect("core agent id");

        let auto = parse_json(
            &register_agent(
                &ctx,
                project_key.clone(),
                "wsl2-trial-fleet".to_string(),
                "opus-4.5".to_string(),
                None,
                Some("phase1 R2-25".to_string()),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("no-name register_agent should succeed"),
        );

        assert_ne!(
            auto["id"].as_i64(),
            Some(core_id),
            "no-name registration must never be acked onto an existing agent's row"
        );
        assert!(
            mcp_agent_mail_core::models::is_valid_agent_name(
                auto["name"].as_str().unwrap_or_default()
            ),
            "auto-generated name must be a valid adjective+noun name"
        );

        let pool = get_db_pool().expect("db pool");
        let existing =
            match mcp_agent_mail_db::queries::get_agent(&cx, &pool, project_id, "BlueLake").await {
                asupersync::Outcome::Ok(agent) => agent,
                other => panic!("get_agent(BlueLake) failed: {other:?}"),
            };
        assert_eq!(existing.id, Some(core_id));
        assert_eq!(existing.program, "wsl2-trial-core");
        assert_eq!(existing.task_description, "core work");
    });
}
