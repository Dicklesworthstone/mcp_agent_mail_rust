//! GH#282: project-addressed shared mailboxes, through the real tool entry
//! points.
//!
//! A `project:<slug>` recipient stores the message once with one project
//! delivery. Every other active agent of the project that was registered when
//! it was sent reads it from its inbox (flagged `via: "project"`), with its
//! own lazily-recorded read and acknowledgement state. It is not broadcast:
//! no per-agent recipient rows exist.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    acknowledge_message, ensure_project, fetch_inbox, get_message_delivery_receipt,
    mark_message_read, register_agent, reply_message, send_message, tool_error_code,
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
    let db_path = format!("/tmp/project-mailbox-{suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/project-mailbox-storage-{suffix}");
    let env = [
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
        ("MESSAGING_AUTO_REGISTER_RECIPIENTS", "true"),
        ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
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

/// Ensure the project and return its slug.
async fn project_slug(ctx: &McpContext, project: &str) -> String {
    let created = ensure_project(ctx, project.to_string(), None)
        .await
        .expect("ensure project");
    serde_json::from_str::<Value>(&created).expect("project JSON")["slug"]
        .as_str()
        .expect("project slug")
        .to_string()
}

async fn register(ctx: &McpContext, project: &str, name: &str, policy: &str) {
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
        policy.to_string(),
    )
    .await
    .expect("set contact policy");
}

async fn send(
    ctx: &McpContext,
    project: &str,
    sender: &str,
    to: &[&str],
    cc: Option<&[&str]>,
    bcc: Option<&[&str]>,
    ack_required: bool,
) -> Result<Value, String> {
    let owned = |names: &[&str]| names.iter().map(|name| (*name).to_string()).collect();
    send_message(
        ctx,
        project.to_string(),
        sender.to_string(),
        owned(to),
        "Shared notice".to_string(),
        "Everyone on this project should see this.".to_string(),
        cc.map(owned),
        bcc.map(owned),
        None,
        None,
        None,
        Some(ack_required),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .map(|json| serde_json::from_str(&json).expect("send JSON"))
    .map_err(|error| tool_error_code(&error).unwrap_or("UNKNOWN").to_string())
}

async fn inbox(
    ctx: &McpContext,
    project: &str,
    agent: &str,
    unread_only: bool,
    mark_read: bool,
) -> Vec<Value> {
    let inbox = fetch_inbox(
        ctx,
        project.to_string(),
        agent.to_string(),
        None,
        None,
        Some(100),
        Some(true),
        Some(unread_only),
        None,
        None,
        Some(mark_read),
    )
    .await
    .expect("fetch inbox");
    serde_json::from_str(&inbox).expect("inbox JSON")
}

/// Message ids and kinds on an agent's durable delivery-event page.
async fn delivery_events(ctx: &McpContext, project: &str, agent: &str) -> Vec<(i64, String)> {
    let page: Value = serde_json::from_str(
        &mcp_agent_mail_tools::fetch_inbox_events(
            ctx,
            project.to_string(),
            agent.to_string(),
            None,
            Some(100),
            None,
        )
        .await
        .expect("fetch inbox events"),
    )
    .expect("events JSON");
    page["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|event| {
            (
                event["message_id"].as_i64().expect("message id"),
                event["kind"].as_str().expect("kind").to_string(),
            )
        })
        .collect()
}

async fn table_count(cx: &Cx, sql: &str) -> i64 {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("DB pool");
    let conn = pool.acquire(cx).await.into_result().expect("DB checkout");
    let rows = conn.query_sync(sql, &[]).expect("count query");
    rows[0].get_as::<i64>(0).expect("count")
}

#[test]
#[allow(clippy::too_many_lines)]
fn project_mailbox_send_is_one_delivery_read_by_each_present_agent() {
    run_with_storage(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project = format!("/tmp/project-mailbox-{}", unique_suffix());
        let slug = project_slug(&ctx, &project).await;
        let address = format!("project:{slug}");
        register(&ctx, &project, "GreenCastle", "open").await;
        register(&ctx, &project, "BlueLake", "open").await;
        register(&ctx, &project, "RedStone", "auto").await;
        register(&ctx, &project, "OrangeCreek", "block_all").await;

        let sent = send(&ctx, &project, "GreenCastle", &[&address], None, None, true)
            .await
            .expect("send to the project mailbox");
        let payload = &sent["deliveries"][0]["payload"];
        assert_eq!(payload["to"], serde_json::json!([address]));
        let message_id = payload["id"].as_i64().expect("message id");

        // One message, one project delivery, no per-agent rows.
        assert_eq!(table_count(&cx, "SELECT COUNT(*) FROM messages").await, 1);
        assert_eq!(
            table_count(&cx, "SELECT COUNT(*) FROM message_recipients").await,
            0
        );
        assert_eq!(
            table_count(&cx, "SELECT COUNT(*) FROM project_mailbox_deliveries").await,
            1
        );

        // An agent that joins after the send is not handed the backlog.
        register(&ctx, &project, "PurpleHill", "open").await;

        for agent in ["BlueLake", "RedStone"] {
            let rows = inbox(&ctx, &project, agent, false, false).await;
            assert_eq!(rows.len(), 1, "{agent} sees the project message: {rows:?}");
            assert_eq!(rows[0]["id"], message_id);
            assert_eq!(rows[0]["via"], "project");
            assert_eq!(rows[0]["kind"], "project");
            assert!(rows[0].get("read_ts").is_none());
            assert_eq!(
                rows[0]["body_md"],
                "Everyone on this project should see this."
            );
        }
        for agent in ["GreenCastle", "PurpleHill", "OrangeCreek"] {
            assert!(
                inbox(&ctx, &project, agent, false, false).await.is_empty(),
                "{agent} must not see the project message"
            );
        }
        // Reading the shared mailbox never materializes receipts.
        assert_eq!(
            table_count(&cx, "SELECT COUNT(*) FROM project_mailbox_receipts").await,
            0
        );

        // Restart-safe monitors see the delivery once, under their own cursor,
        // from one ledger event rather than a row per agent.
        for agent in ["BlueLake", "RedStone"] {
            assert_eq!(
                delivery_events(&ctx, &project, agent).await,
                vec![(message_id, "project".to_string())],
                "{agent}"
            );
        }
        for agent in ["GreenCastle", "PurpleHill", "OrangeCreek"] {
            assert!(
                delivery_events(&ctx, &project, agent).await.is_empty(),
                "{agent} has no delivery event"
            );
        }
        assert_eq!(
            table_count(&cx, "SELECT COUNT(*) FROM inbox_delivery_events").await,
            1
        );

        // BlueLake acknowledges; only BlueLake's state changes.
        let acked: Value = serde_json::from_str(
            &acknowledge_message(
                &ctx,
                project.clone(),
                "BlueLake".to_string(),
                message_id,
                None,
            )
            .await
            .expect("acknowledge a project message"),
        )
        .expect("ack JSON");
        assert_eq!(acked["acknowledged"], true);
        let blue = inbox(&ctx, &project, "BlueLake", false, false).await;
        assert!(blue[0]["ack_ts"].is_string() && blue[0]["read_ts"].is_string());
        let red = inbox(&ctx, &project, "RedStone", false, false).await;
        assert!(red[0].get("ack_ts").is_none() && red[0].get("read_ts").is_none());

        // RedStone's consuming fetch records its own read receipt.
        let consumed = inbox(&ctx, &project, "RedStone", false, true).await;
        assert!(consumed[0]["read_ts"].is_string(), "{consumed:?}");
        assert!(
            inbox(&ctx, &project, "RedStone", true, false)
                .await
                .is_empty(),
            "a read project message is no longer unread"
        );
        let receipt_again: Value = serde_json::from_str(
            &mark_message_read(&ctx, project.clone(), "RedStone".to_string(), message_id)
                .await
                .expect("mark a project message read again"),
        )
        .expect("read JSON");
        assert_eq!(receipt_again["read"], true);

        // The sender and a late joiner have no receipt to record.
        for agent in ["GreenCastle", "PurpleHill"] {
            let error =
                acknowledge_message(&ctx, project.clone(), agent.to_string(), message_id, None)
                    .await
                    .expect_err("not visible to this agent");
            assert_eq!(tool_error_code(&error), Some("NOT_FOUND"), "{agent}");
        }

        let receipt: Value = serde_json::from_str(
            &get_message_delivery_receipt(&ctx, project.clone(), message_id)
                .await
                .expect("delivery receipt"),
        )
        .expect("receipt JSON");
        let mailbox = &receipt["project_mailbox"];
        assert_eq!(mailbox["address"], address.as_str());
        assert_eq!(mailbox["visible_agents"], 2);
        assert_eq!(mailbox["read_count"], 2);
        assert_eq!(mailbox["acknowledged_count"], 1);

        let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("DB pool");
        // Open a fresh runtime handle to the persisted file, then resume a
        // cursor obtained before closing that handle. Both eligible viewers
        // share one durable project event, even after receipt writes.
        let resumed_cursor = {
            let reopened = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path())
                .expect("reopen runtime mailbox");
            let viewers = reopened.query_sync(
                "SELECT project_id, id FROM agents WHERE name IN ('BlueLake','RedStone') ORDER BY id",
                &[],
            ).expect("resolve persisted viewers");
            let mut cursors = Vec::new();
            for viewer in viewers {
                let project_id = viewer.get_named::<i64>("project_id").unwrap();
                let agent_id = viewer.get_named::<i64>("id").unwrap();
                let page = mcp_agent_mail_db::sync::inbox_delivery_events_from_conn(
                    &reopened, project_id, agent_id, None, 100,
                )
                .expect("read shared event after reopening the file");
                assert_eq!(page.events.len(), 1);
                assert_eq!(page.events[0].message_id, message_id);
                cursors.push((project_id, agent_id, page.next_cursor));
            }
            assert_eq!(cursors[0].2, cursors[1].2, "one event serves both viewers");
            cursors
        };
        let reopened = mcp_agent_mail_db::DbConn::open_file(pool.sqlite_path())
            .expect("reopen for cursor continuation");
        for (project_id, agent_id, cursor) in resumed_cursor {
            let page = mcp_agent_mail_db::sync::inbox_delivery_events_from_conn(
                &reopened,
                project_id,
                agent_id,
                Some(cursor),
                100,
            )
            .expect("resume saved shared mailbox cursor");
            assert!(
                page.events.is_empty(),
                "restart must not repeat a consumed event"
            );
        }
        drop(reopened);

        // Canonical SQLite is the FK oracle. Use the production same-engine
        // backup path first: never attach a canonical handle to the live
        // FrankenSQLite-managed inode just to validate it.
        let backup = pool
            .create_proactive_backup(std::time::Duration::ZERO)
            .expect("capture an isolated consistent mailbox backup")
            .expect("file-backed backup");
        let canonical = mcp_agent_mail_db::CanonicalDbConn::open_file(backup.display().to_string())
            .expect("open only the private backup with canonical SQLite");
        assert!(
            canonical
                .query_sync("PRAGMA foreign_key_check", &[])
                .expect("canonical FK check after actual project send")
                .is_empty()
        );
        assert_eq!(canonical.query_sync(
            "SELECT COUNT(*) FROM inbox_delivery_events WHERE agent_id IS NULL AND kind = 'project'",
            &[],
        ).unwrap()[0].get_as::<i64>(0).unwrap(), 1);

        // Restore the historical defect only in the private oracle copy. A
        // green check above must mean valid FKs, not a missing/disabled check.
        canonical
            .execute_raw("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        canonical
            .execute_raw("UPDATE inbox_delivery_events SET agent_id = 0 WHERE kind = 'project'")
            .unwrap();
        let broken = canonical
            .query_sync("PRAGMA foreign_key_check", &[])
            .unwrap();
        assert_eq!(broken.len(), 1);
        assert_eq!(
            broken[0].get_as::<String>(0).unwrap(),
            "inbox_delivery_events"
        );
        assert_eq!(broken[0].get_as::<String>(2).unwrap(), "agents");
    });
}

#[test]
fn project_mailbox_addresses_are_validated_and_combine_with_direct_recipients() {
    run_with_storage(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project = format!("/tmp/project-mailbox-{}", unique_suffix());
        let other = format!("/tmp/project-mailbox-other-{}", unique_suffix());
        let slug = project_slug(&ctx, &project).await;
        let other_slug = project_slug(&ctx, &other).await;
        let address = format!("project:{slug}");
        register(&ctx, &project, "GreenCastle", "open").await;
        register(&ctx, &project, "BlueLake", "open").await;
        register(&ctx, &project, "RedStone", "open").await;

        for (to, bcc) in [
            (vec![format!("project:{other_slug}")], None),
            (vec!["project:".to_string()], None),
            (vec!["BlueLake".to_string()], Some(vec![address.clone()])),
        ] {
            let to: Vec<&str> = to.iter().map(String::as_str).collect();
            let bcc: Option<Vec<&str>> = bcc
                .as_ref()
                .map(|names| names.iter().map(String::as_str).collect());
            let error = send(
                &ctx,
                &project,
                "GreenCastle",
                &to,
                None,
                bcc.as_deref(),
                false,
            )
            .await
            .expect_err("invalid project mailbox address");
            assert_eq!(error, "INVALID_ARGUMENT", "to={to:?} bcc={bcc:?}");
        }
        assert_eq!(table_count(&cx, "SELECT COUNT(*) FROM messages").await, 0);

        // Addressing the project by its human key, alongside a direct
        // recipient: the direct delivery wins for that agent, so nobody sees
        // the message twice.
        let human_key_address = format!("project:{project}");
        let sent = send(
            &ctx,
            &project,
            "GreenCastle",
            &["BlueLake"],
            Some(&[&human_key_address]),
            None,
            false,
        )
        .await
        .expect("direct + cc project mailbox");
        let payload = &sent["deliveries"][0]["payload"];
        assert_eq!(payload["to"], serde_json::json!(["BlueLake"]));
        assert_eq!(payload["cc"], serde_json::json!([address]));
        let message_id = payload["id"].as_i64().expect("message id");

        let blue = inbox(&ctx, &project, "BlueLake", false, false).await;
        assert_eq!(blue.len(), 1);
        assert_eq!(blue[0]["kind"], "to");
        assert!(blue[0].get("via").is_none());
        let red = inbox(&ctx, &project, "RedStone", false, false).await;
        assert_eq!(red.len(), 1);
        assert_eq!(red[0]["id"], message_id);
        assert_eq!(red[0]["via"], "project");

        // A reply can address the project mailbox too.
        let reply: Value = serde_json::from_str(
            &reply_message(
                &ctx,
                project.clone(),
                message_id,
                "RedStone".to_string(),
                "Acknowledged for the whole team.".to_string(),
                Some(vec![address.clone()]),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("reply to the project mailbox"),
        )
        .expect("reply JSON");
        let reply_id = reply["id"].as_i64().expect("reply id");
        assert_eq!(reply["reply_to"], message_id);
        for agent in ["GreenCastle", "BlueLake"] {
            let rows = inbox(&ctx, &project, agent, false, false).await;
            assert!(
                rows.iter()
                    .any(|row| row["id"] == reply_id && row["via"] == "project"),
                "{agent} sees the reply through the project mailbox: {rows:?}"
            );
        }
        assert!(
            !inbox(&ctx, &project, "RedStone", false, false)
                .await
                .iter()
                .any(|row| row["id"] == reply_id),
            "the replier does not receive its own project mail"
        );
    });
}
