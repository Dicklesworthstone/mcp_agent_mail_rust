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
//! - exhausted short names spill into fresh identities without reviving retired agents;
//! - explicit-name re-registration keeps its documented upsert semantics.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    claim_fresh_auto_named_agent, create_agent_identity, ensure_project, fetch_inbox,
    register_agent, retire_agent, send_message, set_contact_policy, tool_util::get_db_pool, whois,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
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

fn flush_archive() {
    mcp_agent_mail_storage::wbq_flush();
    assert!(
        mcp_agent_mail_storage::archive_backlog_flush_blocking(std::time::Duration::from_secs(10)),
        "archive fallback backlog must drain before reading profiles or restoring the environment"
    );
    mcp_agent_mail_storage::flush_async_commits();
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
            let result = rt.block_on(f(cx));
            flush_archive();
            result
        },
    )
}

fn parse_json(payload: &str) -> Value {
    serde_json::from_str(payload).expect("tool response must be valid JSON")
}

async fn agent_snapshot(
    cx: &Cx,
    pool: &mcp_agent_mail_db::DbPool,
    project_id: i64,
) -> BTreeMap<String, Value> {
    mcp_agent_mail_db::queries::list_agents(cx, pool, project_id)
        .await
        .into_result()
        .expect("read all agent rows, including retired identities")
        .into_iter()
        .map(|agent| {
            (
                agent.name.to_ascii_lowercase(),
                serde_json::to_value(agent).expect("serialize complete agent row"),
            )
        })
        .collect()
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

/// GH#415: even a drawer stuck on an occupied retired name must allocate a
/// fresh identity. Retirement, credentials, and the entire old profile stay intact.
#[test]
fn auto_name_exhaustion_allocates_without_reviving_retired_agent() {
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

        retire_agent(
            &ctx,
            project_key.clone(),
            "BlueLake".to_string(),
            Some(
                core["registration_token"]
                    .as_str()
                    .expect("original credential")
                    .to_string(),
            ),
            None,
            None,
            Some("http".to_string()),
        )
        .await
        .expect("retire the original identity using its own credential");

        let pool = get_db_pool().expect("db pool");
        let before = agent_snapshot(&cx, &pool, project_id).await;
        assert!(before["bluelake"]["retired_at"].is_i64());
        let mut short_redraws = 0;
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
            || {
                short_redraws += 1;
                "bluelake".to_string()
            },
        )
        .await
        .expect("exhausted short names must spill into a fresh identity");

        assert!(
            short_redraws <= 3,
            "repeated short names must not extend the four-draw budget"
        );
        assert_ne!(
            row.id,
            Some(core_id),
            "retirement never frees the old identity"
        );
        assert!(row.retired_at.is_none());
        assert!(
            row.name.contains('-'),
            "overflow must leave the short-name pool"
        );
        assert!(mcp_agent_mail_core::models::is_valid_agent_name(&row.name));
        assert_eq!(row.program, "wsl2-trial-fleet");
        assert_eq!(row.task_description, "phase1 R2-25");
        let after = agent_snapshot(&cx, &pool, project_id).await;
        assert_eq!(after["bluelake"], before["bluelake"]);
        assert_eq!(after.len(), before.len() + 1);
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
            "auto-generated name must be a valid agent identifier"
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

/// GH#415: both public identity tools must remain usable after every short
/// adjective+noun name is occupied. Most occupants are retired, as in the
/// reporter's long-lived project, and none may be reassigned or revived.
#[test]
#[allow(clippy::too_many_lines)]
fn identity_tools_work_after_full_short_name_exhaustion() {
    run_serial_async(|cx| async move {
        let project_key = format!("/tmp/exhausted-identities-{}", unique_suffix());
        let ctx = McpContext::new(cx.clone(), 1);
        let project = parse_json(
            &ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project"),
        );
        let project_id = project["id"].as_i64().expect("project id");

        let taken: Vec<String> = mcp_agent_mail_core::models::VALID_ADJECTIVES
            .iter()
            .flat_map(|adjective| {
                mcp_agent_mail_core::models::VALID_NOUNS
                    .iter()
                    .map(move |noun| format!("{adjective}{noun}"))
            })
            .map(|raw| {
                mcp_agent_mail_core::models::normalize_agent_name(&raw)
                    .expect("every generator vocabulary pair is valid")
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let pool = get_db_pool().expect("db pool");
        let conn = pool.acquire(&cx).await.into_result().expect("checkout");
        conn.execute_raw("BEGIN IMMEDIATE").expect("begin seed");
        for chunk in taken.chunks(200) {
            let values = chunk
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let retired_at = if index % 5 == 0 { "NULL" } else { "42" };
                    format!(
                        "({project_id}, '{name}', 'seed', 'seed', 'original task', \
                         1, 1, 'file', 'block_all', 1, 'original-{name}', {retired_at})"
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            conn.execute_raw(&format!(
                "INSERT INTO agents (project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts, attachments_policy, contact_policy, \
                 reaper_exempt, registration_token, retired_at) VALUES {values}"
            ))
            .expect("seed taken names");
        }
        conn.execute_raw("COMMIT").expect("commit seed");
        drop(conn);
        let before = agent_snapshot(&cx, &pool, project_id).await;
        assert_eq!(
            before.len(),
            taken.len(),
            "the entire short-name pool is occupied"
        );

        let registered = parse_json(
            &register_agent(
                &ctx,
                project_key.clone(),
                "register-fleet".to_string(),
                "opus-4.5".to_string(),
                None,
                Some("fresh registration".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("register_agent must outgrow the fully occupied vocabulary"),
        );
        let created = parse_json(
            &create_agent_identity(
                &ctx,
                project_key.clone(),
                "create-fleet".to_string(),
                "opus-4.5".to_string(),
                None,
                Some("fresh identity".to_string()),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("create_agent_identity must outgrow the fully occupied vocabulary"),
        );
        assert_ne!(registered["id"], created["id"]);
        assert_ne!(registered["name"], created["name"]);
        assert_ne!(
            registered["registration_token"],
            created["registration_token"]
        );

        flush_archive();
        for identity in [&registered, &created] {
            let name = identity["name"].as_str().expect("agent name");
            let id = identity["id"].as_i64().expect("new agent id");
            assert!(!before.contains_key(&name.to_ascii_lowercase()));
            assert!(name.contains('-'), "all short names are already occupied");
            assert!(mcp_agent_mail_core::models::is_valid_agent_name(name));
            assert!(identity["retired_at"].is_null());
            let persisted = mcp_agent_mail_db::queries::get_agent_by_id_fresh(&cx, &pool, id)
                .await
                .into_result()
                .expect("fresh identity is durably registered");
            let token = identity["registration_token"]
                .as_str()
                .expect("new credential");
            assert!(!token.is_empty());
            assert_eq!(persisted.registration_token.as_deref(), Some(token));
            let profile = parse_json(
                &whois(
                    &ctx,
                    project_key.clone(),
                    name.to_ascii_uppercase(),
                    Some(false),
                    None,
                )
                .await
                .expect("overflow identity resolves case-insensitively"),
            );
            assert_eq!(profile["id"], identity["id"]);
            assert_eq!(profile["name"], identity["name"]);

            let profile_path = Config::get()
                .storage_root
                .join("projects")
                .join(project["slug"].as_str().expect("project slug"))
                .join("agents")
                .join(name)
                .join("profile.json");
            let archived = parse_json(
                &std::fs::read_to_string(profile_path).expect("overflow profile was archived"),
            );
            assert_eq!(archived["name"], identity["name"]);
            assert_eq!(archived["program"], identity["program"]);
        }

        let sender = registered["name"].as_str().expect("sender name");
        let recipient = created["name"].as_str().expect("recipient name");
        set_contact_policy(
            &ctx,
            project_key.clone(),
            recipient.to_string(),
            "open".to_string(),
        )
        .await
        .expect("open the fresh recipient to direct mail");
        let sent = parse_json(
            &send_message(
                &ctx,
                project_key.clone(),
                sender.to_ascii_lowercase(),
                vec![recipient.to_ascii_uppercase()],
                "Overflow identities can communicate".to_string(),
                "Fresh mail after vocabulary exhaustion.".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(
                    registered["registration_token"]
                        .as_str()
                        .expect("sender token")
                        .to_string(),
                ),
                None,
            )
            .await
            .expect("an overflow identity can send with its own credential"),
        );
        assert_eq!(sent["verified_sender"], true);
        assert_eq!(sent["count"], 1);
        let inbox = parse_json(
            &fetch_inbox(
                &ctx,
                project_key.clone(),
                recipient.to_ascii_uppercase(),
                None,
                None,
                None,
                Some(true),
                None,
                None,
                None,
                Some(false),
            )
            .await
            .expect("overflow recipient can retrieve its mail"),
        );
        let messages = inbox.as_array().expect("inbox messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["from"], registered["name"]);
        assert_eq!(
            messages[0]["body_md"],
            "Fresh mail after vocabulary exhaustion."
        );

        let resumed = register_explicit(
            &ctx,
            &project_key,
            &recipient.to_ascii_uppercase(),
            "resumed-worker",
            "next task",
        )
        .await;
        assert_eq!(resumed["id"], created["id"]);
        assert_eq!(resumed["name"], created["name"]);
        assert_eq!(resumed["program"], "resumed-worker");

        let mut after = agent_snapshot(&cx, &pool, project_id).await;
        assert_eq!(after.len(), before.len() + 2);
        after.remove(&sender.to_ascii_lowercase());
        after.remove(&recipient.to_ascii_lowercase());
        for (name, original) in &before {
            assert_eq!(
                after.get(name),
                Some(original),
                "existing identity {name}, its credential and lifecycle state must be unchanged"
            );
        }
    });
}
