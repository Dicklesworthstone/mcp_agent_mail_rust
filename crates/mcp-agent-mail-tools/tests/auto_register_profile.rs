//! GH#301: a `send_message` to an unregistered same-project recipient
//! auto-registers a placeholder agent (when `MESSAGING_AUTO_REGISTER_RECIPIENTS`
//! is on and the registration proof gate is off). That placeholder must exist
//! in the Git archive too, or DB and archive agent inventories drift by one and
//! a reconstruct cannot recreate it.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    ensure_project, fetch_inbox, list_agents, macro_contact_handshake, register_agent,
    reply_message, respond_contact, send_message,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    F: FnOnce(Cx, String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let db_path = format!("/tmp/auto-register-profile-{suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/auto-register-profile-storage-{suffix}");
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
            f(cx, storage_root.clone()).await
        });
        Config::reset_cached();
        out
    })
}

fn find_agent_profiles(storage_root: &str, agent: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, agent: &str, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                walk(&path, agent, found);
            } else if path.file_name().is_some_and(|n| n == "profile.json")
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == agent)
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(Path::new(storage_root), agent, &mut found);
    found
}

#[test]
fn auto_registered_recipient_gets_an_archived_profile() {
    run_with_storage(|cx, storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/auto-register-profile-{}", unique_suffix());
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");
        register_agent(
            &ctx,
            project_key.clone(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some("GreenCastle".to_string()),
            Some("sender".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register sender");
        assert!(
            find_agent_profiles(&storage_root, "CobaltRobin").is_empty(),
            "the recipient must not exist before the send"
        );

        let sent = send_message(
            &ctx,
            project_key.clone(),
            "GreenCastle".to_string(),
            vec!["CobaltRobin".to_string()],
            "hello".to_string(),
            "body".to_string(),
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
        .expect("send to an unregistered recipient auto-registers it");
        let sent: Value = serde_json::from_str(&sent).expect("send JSON");
        assert!(sent.get("deliveries").is_some(), "send reply: {sent}");

        mcp_agent_mail_storage::wbq_flush();
        let profiles = find_agent_profiles(&storage_root, "CobaltRobin");
        assert_eq!(
            profiles.len(),
            1,
            "the auto-registered placeholder must have exactly one archived profile: {profiles:?}"
        );
        let profile: Value =
            serde_json::from_str(&std::fs::read_to_string(&profiles[0]).expect("read profile"))
                .expect("profile JSON");
        assert_eq!(profile["name"], "CobaltRobin");
        assert_eq!(profile["program"], "unknown");
        assert_eq!(profile["model"], "unknown");
    });
}

/// br-kp1in.15: after a cross-project contact handshake, sending to the linked
/// agent's name used to auto-register a same-name placeholder in the SENDER's
/// project and report the message persisted while the real peer received
/// nothing. It must now refuse with `CROSS_PROJECT_RECIPIENT` and write nothing,
/// and the handshake must say its welcome was not delivered.
#[test]
fn send_to_cross_project_contact_is_refused_instead_of_misdelivered() {
    run_with_storage(|cx, _storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_a = format!("/tmp/xproj-a-{}", unique_suffix());
        let project_b = format!("/tmp/xproj-b-{}", unique_suffix());
        for (project, name) in [(&project_a, "GreenCastle"), (&project_b, "BronzeHare")] {
            ensure_project(&ctx, project.clone(), None)
                .await
                .expect("ensure_project");
            register_agent(
                &ctx,
                project.clone(),
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
        }

        let handshake = macro_contact_handshake(
            &ctx,
            project_a.clone(),
            Some("GreenCastle".to_string()),
            Some("BronzeHare".to_string()),
            None,
            None,
            Some(project_b.clone()),
            Some("cross-repo work".to_string()),
            Some(true),
            None,
            Some("hello".to_string()),
            Some("welcome across repos".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("cross-project handshake");
        let handshake: Value = serde_json::from_str(&handshake).expect("handshake JSON");
        assert!(handshake["welcome_message"].is_null(), "{handshake}");
        assert!(
            handshake["welcome_skipped_reason"]
                .as_str()
                .is_some_and(|r| r.starts_with("cross_project_messaging_unsupported")),
            "the skipped welcome must be reported: {handshake}"
        );

        let err = send_message(
            &ctx,
            project_a.clone(),
            "GreenCastle".to_string(),
            vec!["BronzeHare".to_string()],
            "to the linked peer".to_string(),
            "body".to_string(),
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
        .expect_err("a cross-project contact name must not be auto-registered locally");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
            Some("CROSS_PROJECT_RECIPIENT"),
            "{err:?}"
        );

        let agents_a: Value = serde_json::from_str(
            &list_agents(&ctx, project_a.clone(), None, None)
                .await
                .expect("list agents in A"),
        )
        .expect("agents JSON");
        let names: Vec<&str> = agents_a
            .as_array()
            .expect("agents array")
            .iter()
            .filter_map(|agent| agent["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["GreenCastle"],
            "no BronzeHare placeholder may be created in the sender's project"
        );
    });
}

async fn register_open_agent(ctx: &McpContext, project: &str, name: &str) -> i64 {
    ensure_project(ctx, project.to_string(), None)
        .await
        .expect("ensure project");
    let agent = register_agent(
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
    .expect("open local contact policy");
    serde_json::from_str::<Value>(&agent).expect("agent JSON")["id"]
        .as_i64()
        .expect("agent id")
}

async fn peek_inbox(ctx: &McpContext, project: &str, agent: &str) -> Vec<Value> {
    let inbox = fetch_inbox(
        ctx,
        project.to_string(),
        agent.to_string(),
        None,
        None,
        Some(100),
        Some(true),
        None,
        None,
        None,
        Some(false),
    )
    .await
    .expect("peek inbox");
    serde_json::from_str(&inbox).expect("inbox JSON")
}

async fn delivery_counts(cx: &Cx) -> (i64, i64, i64) {
    let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("DB pool");
    let conn = pool.acquire(cx).await.into_result().expect("DB checkout");
    let rows = conn
        .query_sync(
            "SELECT (SELECT COUNT(*) FROM messages) AS messages, \
             (SELECT COUNT(*) FROM message_recipients) AS recipients, \
             (SELECT COUNT(*) FROM agents) AS agents",
            &[],
        )
        .expect("delivery counts");
    (
        rows[0].get_named("messages").expect("message count"),
        rows[0].get_named("recipients").expect("recipient count"),
        rows[0].get_named("agents").expect("agent count"),
    )
}

fn archive_delivery_files(storage_root: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !path.file_name().is_some_and(|name| name == ".git") {
                    walk(&path, files);
                }
            } else {
                // Includes messages, inbox/outbox copies, thread digests,
                // profiles and attachment bytes; excludes Git bookkeeping.
                files.insert(path.clone(), std::fs::read(path).expect("archive file"));
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(&Path::new(storage_root).join("projects"), &mut files);
    files
}

#[test]
fn default_reply_preserves_foreign_sender_identity_before_any_delivery_effects() {
    run_with_storage(|cx, storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        for scenario in ["namesake", "blocked", "alias"] {
            let project_a = format!("/tmp/reply-source-{scenario}-{}", unique_suffix());
            let project_b = format!("/tmp/reply-target-{scenario}-{}", unique_suffix());
            let foreign_sender = register_open_agent(&ctx, &project_a, "GreenCastle").await;
            register_open_agent(&ctx, &project_b, "BronzeHare").await;
            if scenario != "blocked" {
                let local_namesake = register_open_agent(&ctx, &project_b, "GreenCastle").await;
                assert_ne!(foreign_sender, local_namesake);
            }
            macro_contact_handshake(
                &ctx,
                project_a.clone(),
                Some("GreenCastle".to_string()),
                Some("BronzeHare".to_string()),
                None,
                None,
                Some(project_b.clone()),
                Some("reply identity regression".to_string()),
                Some(true),
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
            .expect("real cross-project contact notice");
            let inbox = peek_inbox(&ctx, &project_b, "BronzeHare").await;
            let notice_id = inbox[0]["id"].as_i64().expect("contact notice id");
            assert_eq!(inbox[0]["from"], "GreenCastle");
            if scenario == "blocked" {
                respond_contact(
                    &ctx,
                    project_b.clone(),
                    "BronzeHare".to_string(),
                    "GreenCastle".to_string(),
                    Some(project_a.clone()),
                    false,
                    None,
                )
                .await
                .expect("block original contact after receiving its notice");
            } else if scenario == "alias" {
                // Model legacy project rows whose paths resolve to the same
                // directory. Their distinct sender IDs still cannot be swapped.
                std::fs::create_dir_all(&project_b).expect("create aliased project directory");
                assert_eq!(
                    mcp_agent_mail_core::identity::resolve_project_path(&project_b),
                    mcp_agent_mail_core::identity::resolve_project_path(&format!("{project_b}/."))
                );
                let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
                let conn = pool.acquire(&cx).await.into_result().expect("checkout");
                conn.execute_sync(
                    "UPDATE projects SET human_key = ? WHERE id = \
                     (SELECT project_id FROM agents WHERE id = ?)",
                    &[
                        mcp_agent_mail_db::sqlmodel_core::Value::Text(format!("{project_b}/.")),
                        mcp_agent_mail_db::sqlmodel_core::Value::BigInt(foreign_sender),
                    ],
                )
                .expect("legacy project path alias");
            }

            mcp_agent_mail_storage::wbq_flush();
            let before_counts = delivery_counts(&cx).await;
            let before_archive = archive_delivery_files(&storage_root);
            assert!(
                !before_archive.is_empty(),
                "contact notice must be archived"
            );
            let attachment_path = format!("{storage_root}/private-reply.txt");
            std::fs::write(&attachment_path, "private attachment bytes")
                .expect("existing attachment fixture");
            for attachment_paths in [
                None,
                Some(vec![attachment_path]),
                Some(vec![format!("{storage_root}/absent.png")]),
            ] {
                let err = reply_message(
                    &ctx,
                    project_b.clone(),
                    notice_id,
                    "BronzeHare".to_string(),
                    "private reply for the original sender".to_string(),
                    None,
                    Some(vec!["CobaltRobin".to_string()]),
                    Some(vec!["SilverFox".to_string()]),
                    None,
                    None,
                    None,
                    attachment_paths,
                    None,
                    None,
                    None,
                )
                .await
                .expect_err("implicit reply must preserve the known original sender identity");
                assert_eq!(
                    mcp_agent_mail_tools::tool_util::tool_error_code(&err),
                    Some("CROSS_PROJECT_RECIPIENT"),
                    "{scenario}: {err:?}"
                );
                assert!(
                    !format!("{err:?}").contains(&project_a),
                    "the refusal must not disclose the foreign project's path"
                );
                assert_eq!(delivery_counts(&cx).await, before_counts, "{scenario}");
                mcp_agent_mail_storage::wbq_flush();
                assert_eq!(
                    archive_delivery_files(&storage_root),
                    before_archive,
                    "{scenario}"
                );
            }

            // Explicit local destinations are intentional, including an empty
            // To list that opts into CC/BCC-only delivery. No implicit foreign
            // default may leak back into either successful routing form.
            register_open_agent(&ctx, &project_b, "CobaltRobin").await;
            register_open_agent(&ctx, &project_b, "SilverFox").await;
            for to in [vec!["BronzeHare".to_string()], vec![]] {
                let reply = reply_message(
                    &ctx,
                    project_b.clone(),
                    notice_id,
                    "BronzeHare".to_string(),
                    "intentionally routed locally".to_string(),
                    Some(to.clone()),
                    Some(vec!["CobaltRobin".to_string()]),
                    Some(vec!["SilverFox".to_string()]),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .expect("explicit local reroute remains supported");
                let reply: Value = serde_json::from_str(&reply).expect("reply JSON");
                assert_eq!(reply["to"], serde_json::json!(to));
                assert_eq!(reply["cc"], serde_json::json!(["CobaltRobin"]));
                assert_eq!(reply["bcc"], serde_json::json!(["SilverFox"]));
            }
            assert_eq!(peek_inbox(&ctx, &project_b, "CobaltRobin").await.len(), 2);
            assert_eq!(peek_inbox(&ctx, &project_b, "SilverFox").await.len(), 2);
            if scenario != "blocked" {
                assert!(peek_inbox(&ctx, &project_b, "GreenCastle").await.is_empty());
            }
        }
    });
}

#[test]
fn committed_default_reply_replays_before_changed_parent_identity_is_refused() {
    run_with_storage(|cx, storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project = format!("/tmp/reply-replay-local-{}", unique_suffix());
        let remote = format!("/tmp/reply-replay-remote-{}", unique_suffix());
        let local_sender = register_open_agent(&ctx, &project, "GreenCastle").await;
        let replier = register_open_agent(&ctx, &project, "BronzeHare").await;
        let foreign_sender = register_open_agent(&ctx, &remote, "GreenCastle").await;
        let parent_id = {
            let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
            let project_id = mcp_agent_mail_db::queries::get_agent_by_id_fresh(&cx, &pool, replier)
                .await
                .into_result()
                .expect("reply agent")
                .project_id;
            mcp_agent_mail_db::queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                local_sender,
                "local parent",
                "body",
                None,
                "normal",
                false,
                "[]",
                &[(replier, "to")],
            )
            .await
            .into_result()
            .expect("real local parent")
            .id
            .expect("parent id")
        };
        let reply = || {
            reply_message(
                &ctx,
                project.clone(),
                parent_id,
                "BronzeHare".to_string(),
                "same committed reply".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some("reply-original-identity".to_string()),
            )
        };
        let original: Value = serde_json::from_str(&reply().await.expect("local default reply"))
            .expect("original reply JSON");
        assert_eq!(original["to"], serde_json::json!(["GreenCastle"]));
        {
            // Simulate authoritative repair changing the parent identity after
            // this operation committed. An existing receipt must remain usable.
            let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
            let conn = pool.acquire(&cx).await.into_result().expect("checkout");
            conn.execute_sync(
                "UPDATE messages SET sender_id = ? WHERE id = ?",
                &[
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(foreign_sender),
                    mcp_agent_mail_db::sqlmodel_core::Value::BigInt(parent_id),
                ],
            )
            .expect("repair parent identity");
        }
        mcp_agent_mail_storage::wbq_flush();
        let before_counts = delivery_counts(&cx).await;
        let before_archive = archive_delivery_files(&storage_root);
        let replay: Value = serde_json::from_str(&reply().await.expect("committed reply replay"))
            .expect("replayed reply JSON");
        assert_eq!(replay["id"], original["id"]);
        assert_eq!(replay["idempotent_replay"], true);
        let fresh_err = reply_message(
            &ctx,
            project,
            parent_id,
            "BronzeHare".to_string(),
            "new default reply".to_string(),
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
        )
        .await
        .expect_err("fresh reply cannot reuse the repaired sender's local namesake");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&fresh_err),
            Some("CROSS_PROJECT_RECIPIENT")
        );
        assert_eq!(delivery_counts(&cx).await, before_counts);
        mcp_agent_mail_storage::wbq_flush();
        assert_eq!(archive_delivery_files(&storage_root), before_archive);
    });
}
