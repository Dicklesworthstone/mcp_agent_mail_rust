//! GH#334: `last_active_ts` must follow what an agent does, not only when it
//! registered. `list_agents` sorts by it and `active_within_days` filters on
//! it, so an agent that works all day must not rank below (or drop out
//! behind) a newer idle registration.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_db::sqlmodel_core::Value as SqlValue;
use mcp_agent_mail_tools::{
    ensure_project, fetch_inbox, list_agents, register_agent, send_message,
};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

const DAY_MICROS: i64 = 86_400 * 1_000_000;

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    u64::try_from(micros)
        .unwrap_or(u64::MAX)
        .wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_with_storage<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let database_url = format!("sqlite:///tmp/agent-activity-{suffix}.sqlite3");
    let storage_root = format!("/tmp/agent-activity-storage-{suffix}");
    let env = [
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(async {
            let cx = Cx::current().expect("runtime installs the tool test context");
            f(cx).await
        });
        Config::reset_cached();
        out
    })
}

async fn register(ctx: &McpContext, project: &str, name: &str) {
    register_agent(
        ctx,
        project.to_string(),
        "codex-cli".to_string(),
        "gpt-5".to_string(),
        Some(name.to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("register agent");
    mcp_agent_mail_tools::contacts::set_contact_policy(
        ctx,
        project.to_string(),
        name.to_string(),
        "open".to_string(),
    )
    .await
    .expect("open contact policy");
}

/// Directly age an agent's timestamps, as if it registered `days` ago.
async fn age_agent(cx: &Cx, name: &str, days: i64) {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("DB pool");
    let conn = pool.acquire(cx).await.into_result().expect("DB checkout");
    let ts = mcp_agent_mail_core::timestamps::now_micros() - days * DAY_MICROS;
    conn.execute_sync(
        "UPDATE agents SET inception_ts = ?, last_active_ts = ? WHERE name = ?",
        &[
            SqlValue::BigInt(ts),
            SqlValue::BigInt(ts),
            SqlValue::Text(name.to_string()),
        ],
    )
    .expect("age agent");
}

/// `(inception_ts, last_active_ts)` straight from the database.
async fn activity(cx: &Cx, name: &str) -> (i64, i64) {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("DB pool");
    let conn = pool.acquire(cx).await.into_result().expect("DB checkout");
    let rows = conn
        .query_sync(
            "SELECT inception_ts, last_active_ts FROM agents WHERE name = ?",
            &[SqlValue::Text(name.to_string())],
        )
        .expect("agent activity");
    (
        rows[0].get_named("inception_ts").expect("inception_ts"),
        rows[0].get_named("last_active_ts").expect("last_active_ts"),
    )
}

async fn listed_names(
    ctx: &McpContext,
    project: &str,
    active_within_days: Option<u32>,
) -> Vec<String> {
    let listed: Value = serde_json::from_str(
        &list_agents(ctx, project.to_string(), None, active_within_days)
            .await
            .expect("list_agents"),
    )
    .expect("list_agents JSON");
    listed
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|agent| agent["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn sending_and_reading_mail_mark_the_acting_agent_active() {
    run_with_storage(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project = format!("/tmp/agent-activity-{}", unique_suffix());
        ensure_project(&ctx, project.clone(), None)
            .await
            .expect("ensure_project");
        register(&ctx, &project, "GreenCastle").await;
        register(&ctx, &project, "BlueLake").await;
        // GreenCastle registered a month ago; BlueLake a week ago.
        age_agent(&cx, "GreenCastle", 30).await;
        age_agent(&cx, "BlueLake", 7).await;
        assert_eq!(
            listed_names(&ctx, &project, Some(1)).await,
            Vec::<String>::new(),
            "nobody has been active today yet"
        );

        send_message(
            &ctx,
            project.clone(),
            "GreenCastle".to_string(),
            vec!["BlueLake".to_string()],
            "status".to_string(),
            "still working".to_string(),
            None, // cc
            None, // bcc
            None, // attachment_paths
            None, // convert_images
            None, // importance
            None, // ack_required
            None, // thread_id
            None, // topic
            None, // broadcast
            None, // auto_contact_if_blocked
            None, // sender_token
            None, // idempotency_key
        )
        .await
        .expect("send_message");

        let (sender_inception, sender_active) = activity(&cx, "GreenCastle").await;
        assert!(
            sender_active > sender_inception + 29 * DAY_MICROS,
            "sending must move the sender's last_active_ts"
        );
        let (recipient_inception, recipient_active) = activity(&cx, "BlueLake").await;
        assert_eq!(
            recipient_active, recipient_inception,
            "being sent mail is not activity by the recipient"
        );
        assert_eq!(
            listed_names(&ctx, &project, Some(1)).await,
            vec!["GreenCastle".to_string()],
            "the sender is active today"
        );
        assert_eq!(
            listed_names(&ctx, &project, None).await,
            vec!["GreenCastle".to_string(), "BlueLake".to_string()],
            "most recently active first, not most recently registered"
        );

        // Reading the inbox is activity by the reader.
        fetch_inbox(
            &ctx,
            project.clone(),
            "BlueLake".to_string(),
            None,
            None,
            Some(10),
            Some(false),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("fetch_inbox");
        let (_, reader_active) = activity(&cx, "BlueLake").await;
        assert!(
            reader_active > sender_active,
            "fetch_inbox must move the reader's last_active_ts"
        );
        assert_eq!(
            listed_names(&ctx, &project, None).await,
            vec!["BlueLake".to_string(), "GreenCastle".to_string()],
        );
    });
}
