//! Recipient resolution across projects, through the real tool entry points.
//!
//! GH#301: a `send_message` to an unregistered same-project recipient
//! auto-registers a placeholder agent (when `MESSAGING_AUTO_REGISTER_RECIPIENTS`
//! is on and the registration proof gate is off). That placeholder must exist
//! in the Git archive too, or DB and archive agent inventories drift by one and
//! a reconstruct cannot recreate it.
//!
//! br-kp1in.15 / GH#335: a plain name that belongs to another project is never
//! auto-registered locally, and a project-qualified recipient (`Name@project`)
//! is delivered into that project over an approved contact link.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    acknowledge_message, ensure_project, fetch_inbox, list_agents, macro_contact_handshake,
    register_agent, reply_message, respond_contact, send_message,
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
    run_with_storage_and_env(&[], f)
}

fn run_with_storage_and_env<F, Fut, T>(extra_env: &[(&str, &str)], f: F) -> T
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
    let mut env = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
        ("MESSAGING_AUTO_REGISTER_RECIPIENTS", "true"),
        ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
    ];
    env.extend_from_slice(extra_env);
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
/// agent's plain name used to auto-register a same-name placeholder in the
/// SENDER's project and report the message persisted while the real peer
/// received nothing. A plain name must still refuse with
/// `CROSS_PROJECT_RECIPIENT`, write nothing, and name the qualified address that
/// does reach the peer. The handshake's welcome is delivered into the peer's
/// project over the approved link (GH#335).
#[test]
#[allow(clippy::too_many_lines)]
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
        assert!(
            handshake.get("welcome_skipped_reason").is_none(),
            "{handshake}"
        );
        // Human keys may be canonicalized (e.g. /tmp -> /private/tmp on macOS).
        let b_dir = project_b.rsplit('/').next().expect("project b dir");
        assert!(
            handshake["welcome_message"]["deliveries"][0]["project"]
                .as_str()
                .is_some_and(|project| project.ends_with(b_dir)),
            "the welcome is stored in the peer's project: {handshake}"
        );
        let welcome = peek_inbox(&ctx, &project_b, "BronzeHare").await;
        assert!(
            welcome
                .iter()
                .any(|m| m["subject"] == "hello" && m["from"] == "GreenCastle"),
            "the peer receives the welcome in its own inbox: {welcome:?}"
        );
        let before = delivery_counts(&cx).await;

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
        assert!(
            err.data.as_ref().expect("error data")["error"]["data"]["qualified_recipient"]
                .as_str()
                .is_some_and(|q| q.starts_with("BronzeHare@") && q.ends_with(b_dir)),
            "{err:?}"
        );
        assert_eq!(delivery_counts(&cx).await, before, "nothing may be written");

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

/// GH#335: without any contact link, a recipient name registered only in
/// another project of the same product used to be auto-registered as a local
/// placeholder, so the send "succeeded" and the real agent never saw it. It is
/// refused now; a name unknown to every linked project still auto-registers.
#[test]
fn send_to_product_peer_name_is_refused_instead_of_misdelivered() {
    run_with_storage_and_env(
        &[("WORKTREES_ENABLED", "true")],
        |cx, _storage_root| async move {
            let ctx = McpContext::new(cx.clone(), 1);
            let project_a = format!("/tmp/xprod-a-{}", unique_suffix());
            let project_b = format!("/tmp/xprod-b-{}", unique_suffix());
            register_open_agent(&ctx, &project_a, "GreenCastle").await;
            register_open_agent(&ctx, &project_b, "RedStone").await;
            let product = format!("xprod-{}", unique_suffix());
            mcp_agent_mail_tools::ensure_product(&ctx, Some(product.clone()), None)
                .await
                .expect("ensure_product");
            for project in [&project_a, &project_b] {
                mcp_agent_mail_tools::products_link(&ctx, product.clone(), project.clone())
                    .await
                    .expect("products_link");
            }

            let send = |to: &str| {
                send_message(
                    &ctx,
                    project_a.clone(),
                    "GreenCastle".to_string(),
                    vec![to.to_string()],
                    "api change".to_string(),
                    "please bump /v2".to_string(),
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
            };
            let before = delivery_counts(&cx).await;
            // Case-insensitive, like every other agent-name lookup.
            let err = send("redstone")
                .await
                .expect_err("a product peer's name must not be auto-registered locally");
            assert_eq!(
                mcp_agent_mail_tools::tool_util::tool_error_code(&err),
                Some("CROSS_PROJECT_RECIPIENT"),
                "{err:?}"
            );
            let data = &err.data.as_ref().expect("error data")["error"]["data"];
            assert_eq!(data["recipient"], "RedStone", "{err:?}");
            let b_dir = project_b.rsplit('/').next().expect("project b dir");
            assert!(
                data["recipient_project"]
                    .as_str()
                    .is_some_and(|project| project.ends_with(b_dir)),
                "{err:?}"
            );
            assert_eq!(delivery_counts(&cx).await, before, "nothing may be written");
            assert!(
                peek_inbox(&ctx, &project_b, "RedStone").await.is_empty(),
                "the real agent received nothing either"
            );

            // A name that no linked project knows keeps the auto-register behavior.
            send("CobaltRobin")
                .await
                .expect("an unknown name still auto-registers");
            let agents_a: Value = serde_json::from_str(
                &list_agents(&ctx, project_a.clone(), None, None)
                    .await
                    .expect("list agents in A"),
            )
            .expect("agents JSON");
            let mut names: Vec<&str> = agents_a
                .as_array()
                .expect("agents array")
                .iter()
                .filter_map(|agent| agent["name"].as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(names, vec!["CobaltRobin", "GreenCastle"]);
        },
    );
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
                if path.file_name().is_none_or(|name| name != ".git") {
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

/// A default reply to a message from another project's agent goes to that
/// agent in its own project (GH#335), over the approved contact link, and never
/// to a same-name agent in the replier's project. A blocked link, or a reply
/// that would also address local agents, is refused before any write.
#[test]
#[allow(clippy::too_many_lines)]
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
            let reply_to_notice =
                |to: Option<Vec<String>>,
                 cc: Option<Vec<String>>,
                 attachment_paths: Option<Vec<String>>| {
                    reply_message(
                        &ctx,
                        project_b.clone(),
                        notice_id,
                        "BronzeHare".to_string(),
                        "private reply for the original sender".to_string(),
                        to,
                        cc,
                        None,
                        None,
                        None,
                        None,
                        attachment_paths,
                        None,
                        None,
                        None,
                    )
                };

            // The implicit foreign recipient cannot be combined with local
            // ones: one message is stored in one project.
            let err = reply_to_notice(None, Some(vec!["CobaltRobin".to_string()]), None)
                .await
                .expect_err("a default foreign reply plus a local CC spans two projects");
            assert_eq!(
                mcp_agent_mail_tools::tool_util::tool_error_code(&err),
                Some("INVALID_ARGUMENT"),
                "{scenario}: {err:?}"
            );
            assert!(
                !format!("{err:?}").contains(&project_a),
                "the refusal must not disclose the foreign project's path"
            );
            assert_eq!(delivery_counts(&cx).await, before_counts, "{scenario}");

            if scenario == "blocked" {
                for attachment_paths in [
                    None,
                    Some(vec![attachment_path.clone()]),
                    Some(vec![format!("{storage_root}/absent.png")]),
                ] {
                    let checks_contact = attachment_paths.is_none();
                    let err = reply_to_notice(None, None, attachment_paths)
                        .await
                        .expect_err("a blocked link refuses the default reply");
                    if checks_contact {
                        assert_eq!(
                            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
                            Some("CONTACT_BLOCKED"),
                            "{err:?}"
                        );
                    }
                    assert_eq!(delivery_counts(&cx).await, before_counts);
                    mcp_agent_mail_storage::wbq_flush();
                    assert_eq!(archive_delivery_files(&storage_root), before_archive);
                }
            } else {
                let reply: Value = serde_json::from_str(
                    &reply_to_notice(None, None, None)
                        .await
                        .expect("the default reply reaches the original sender's project"),
                )
                .expect("reply JSON");
                assert_eq!(reply["reply_to"], notice_id, "{scenario}: {reply}");
                assert_eq!(reply["to"], serde_json::json!(["GreenCastle"]));
                let sender_home = mcp_agent_mail_db::queries::get_agent_by_id_fresh(
                    &cx,
                    &mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool"),
                    foreign_sender,
                )
                .await
                .into_result()
                .expect("original sender")
                .project_id;
                assert_eq!(reply["project_id"], sender_home, "{scenario}: {reply}");
                let pool = mcp_agent_mail_tools::tool_util::get_db_pool().expect("pool");
                let conn = pool.acquire(&cx).await.into_result().expect("checkout");
                let rows = conn
                    .query_sync(
                        "SELECT agent_id FROM message_recipients WHERE message_id = ?",
                        &[mcp_agent_mail_db::sqlmodel_core::Value::BigInt(
                            reply["id"].as_i64().expect("reply id"),
                        )],
                    )
                    .expect("reply recipients");
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].get_named::<i64>("agent_id").expect("recipient id"),
                    foreign_sender,
                    "{scenario}: the reply reaches the original sender, not a namesake"
                );
                drop(conn);
                assert_eq!(peek_inbox(&ctx, &project_b, "GreenCastle").await.len(), 0);
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
                assert_eq!(peek_inbox(&ctx, &project_b, "GreenCastle").await.len(), 0);
            }
        }
    });
}

#[test]
#[allow(clippy::too_many_lines)]
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
        let replayed: Value = serde_json::from_str(&reply().await.expect("committed reply replay"))
            .expect("replayed reply JSON");
        assert_eq!(replayed["id"], original["id"]);
        assert_eq!(replayed["idempotent_replay"], true);
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
        // The default reply now addresses the repaired sender in its own
        // project (GH#335); with no contact link to it, nothing is delivered,
        // even though both agents' local contact policy is `open`.
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&fresh_err),
            Some("CONTACT_REQUIRED"),
            "{fresh_err:?}"
        );
        assert_eq!(delivery_counts(&cx).await, before_counts);
        mcp_agent_mail_storage::wbq_flush();
        assert_eq!(archive_delivery_files(&storage_root), before_archive);
    });
}

fn tool_error_data(err: &fastmcp::McpError) -> &Value {
    &err.data.as_ref().expect("error data")["error"]["data"]
}

async fn agent_names(ctx: &McpContext, project: &str) -> Vec<String> {
    let agents: Value = serde_json::from_str(
        &list_agents(ctx, project.to_string(), None, None)
            .await
            .expect("list agents"),
    )
    .expect("agents JSON");
    let mut names: Vec<String> = agents
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|agent| agent["name"].as_str().map(str::to_string))
        .collect();
    names.sort_unstable();
    names
}

/// GH#335: `Name@project` (project slug or human key, or the
/// `project:<project>#Name` form) delivers into the recipient's project, only
/// over an approved contact link, and the recipient's `fetch_inbox`,
/// `acknowledge_message` and `reply_message` work there unchanged. The reply
/// travels back into the sender's project the same way.
#[test]
#[allow(clippy::too_many_lines)]
fn qualified_recipient_is_delivered_into_its_project_over_an_approved_link() {
    run_with_storage(|cx, _storage_root| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let frontend = format!("/tmp/xq-frontend-{}", unique_suffix());
        let backend = format!("/tmp/xq-backend-{}", unique_suffix());
        register_open_agent(&ctx, &frontend, "BlueLake").await;
        register_open_agent(&ctx, &frontend, "RedPeak").await;
        register_open_agent(&ctx, &backend, "GreenCastle").await;
        let backend_row: Value = serde_json::from_str(
            &ensure_project(&ctx, backend.clone(), None)
                .await
                .expect("backend project"),
        )
        .expect("project JSON");
        let backend_slug = backend_row["slug"].as_str().expect("slug").to_string();
        let backend_key = backend_row["human_key"]
            .as_str()
            .expect("human key")
            .to_string();
        let backend_id = backend_row["id"].as_i64().expect("project id");

        let send = |to: Vec<&str>, subject: &str, thread: Option<&str>, key: Option<&str>| {
            send_message(
                &ctx,
                frontend.clone(),
                "BlueLake".to_string(),
                to.into_iter().map(str::to_string).collect(),
                subject.to_string(),
                "Please bump /v2.".to_string(),
                None,
                None,
                None,
                None,
                None,
                Some(true), // ack_required
                thread.map(str::to_string),
                None,
                None,
                None,
                None,
                key.map(str::to_string),
            )
        };
        let qualified = format!("GreenCastle@{backend_slug}");
        let before = delivery_counts(&cx).await;

        // No contact link yet: refused, even though GreenCastle's own contact
        // policy is `open`, and nothing is written.
        let err = send(vec![qualified.as_str()], "API change", None, None)
            .await
            .expect_err("no approved link, no delivery");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
            Some("CONTACT_REQUIRED"),
            "{err:?}"
        );
        assert_eq!(
            tool_error_data(&err)["suggested_tool_calls"][0]["arguments"]["to_project"],
            backend_key.as_str()
        );
        // Unknown project, unknown agent: clear errors, never auto-registered.
        for (to, missing) in [
            (
                "GreenCastle@no-such-project-xq".to_string(),
                "no-such-project-xq",
            ),
            (format!("SilverFox@{backend_slug}"), backend_key.as_str()),
        ] {
            let err = send(vec![to.as_str()], "API change", None, None)
                .await
                .expect_err("unknown qualified recipient");
            assert_eq!(
                mcp_agent_mail_tools::tool_util::tool_error_code(&err),
                Some("RECIPIENT_NOT_FOUND"),
                "{err:?}"
            );
            assert!(
                tool_error_data(&err)["unknown_external"]
                    .get(missing)
                    .is_some(),
                "{err:?}"
            );
        }
        // Malformed qualifier.
        let err = send(vec!["GreenCastle@"], "API change", None, None)
            .await
            .expect_err("empty project part");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
            Some("INVALID_ARGUMENT"),
            "{err:?}"
        );
        assert_eq!(delivery_counts(&cx).await, before, "nothing may be written");
        assert_eq!(agent_names(&ctx, &backend).await, vec!["GreenCastle"]);

        // A pending request carries no welcome yet, and says why.
        let pending: Value = serde_json::from_str(
            &macro_contact_handshake(
                &ctx,
                frontend.clone(),
                Some("BlueLake".to_string()),
                Some("GreenCastle".to_string()),
                None,
                None,
                Some(backend.clone()),
                Some("API coordination".to_string()),
                Some(false),
                None,
                Some("early hello".to_string()),
                Some("not yet".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("pending cross-project handshake"),
        )
        .expect("handshake JSON");
        assert!(pending["welcome_message"].is_null(), "{pending}");
        assert!(
            pending["welcome_skipped_reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("contact_not_approved")),
            "{pending}"
        );

        // Handshake with auto-approval: the welcome is delivered too.
        let handshake: Value = serde_json::from_str(
            &macro_contact_handshake(
                &ctx,
                frontend.clone(),
                Some("BlueLake".to_string()),
                Some("GreenCastle".to_string()),
                None,
                None,
                Some(backend.clone()),
                Some("API coordination".to_string()),
                Some(true),
                None,
                Some("hello".to_string()),
                Some("hi from the frontend".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("cross-project handshake"),
        )
        .expect("handshake JSON");
        assert!(
            handshake.get("welcome_skipped_reason").is_none(),
            "{handshake}"
        );

        // Slug, human key and `project:` forms all resolve to the backend.
        let human_key_form = format!("GreenCastle@{backend_key}");
        let project_form = format!("project:{backend_slug}#GreenCastle");
        let mut sent_ids = Vec::new();
        for (to, subject) in [
            (qualified.as_str(), "API change"),
            (human_key_form.as_str(), "API change 2"),
            (project_form.as_str(), "API change 3"),
        ] {
            let response: Value = serde_json::from_str(
                &send(vec![to], subject, Some("br-124"), None)
                    .await
                    .expect("qualified send over the approved link"),
            )
            .expect("send JSON");
            let delivery = &response["deliveries"][0];
            assert_eq!(delivery["project"], backend_key.as_str(), "{response}");
            assert_eq!(delivery["payload"]["project_id"], backend_id, "{response}");
            assert_eq!(
                delivery["payload"]["to"],
                serde_json::json!(["GreenCastle"])
            );
            sent_ids.push(delivery["payload"]["id"].as_i64().expect("message id"));
        }

        // The recipient reads, acknowledges and replies in its own project.
        let inbox = peek_inbox(&ctx, &backend, "GreenCastle").await;
        let subjects: Vec<&str> = inbox.iter().filter_map(|m| m["subject"].as_str()).collect();
        assert!(!subjects.contains(&"early hello"), "{subjects:?}");
        for subject in ["hello", "API change", "API change 2", "API change 3"] {
            assert!(subjects.contains(&subject), "{subject}: {subjects:?}");
        }
        let api_change = sent_ids[0];
        let acked: Value = serde_json::from_str(
            &acknowledge_message(
                &ctx,
                backend.clone(),
                "GreenCastle".to_string(),
                api_change,
                None,
            )
            .await
            .expect("acknowledge in the recipient's project"),
        )
        .expect("ack JSON");
        assert_eq!(acked["acknowledged"], true, "{acked}");

        let reply: Value = serde_json::from_str(
            &reply_message(
                &ctx,
                backend.clone(),
                api_change,
                "GreenCastle".to_string(),
                "Bumped.".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some("xq-reply".to_string()),
            )
            .await
            .expect("default reply goes back to the sender's project"),
        )
        .expect("reply JSON");
        assert_eq!(reply["reply_to"], api_change, "{reply}");
        assert_eq!(reply["thread_id"], "br-124", "{reply}");
        assert_eq!(reply["to"], serde_json::json!(["BlueLake"]), "{reply}");
        let frontend_row: Value = serde_json::from_str(
            &ensure_project(&ctx, frontend.clone(), None)
                .await
                .expect("frontend project"),
        )
        .expect("project JSON");
        assert_eq!(
            reply["deliveries"][0]["project"], frontend_row["human_key"],
            "{reply}"
        );
        let blue_inbox = peek_inbox(&ctx, &frontend, "BlueLake").await;
        assert!(
            blue_inbox
                .iter()
                .any(|m| m["id"] == reply["id"] && m["from"] == "GreenCastle"),
            "the reply lands in BlueLake's frontend inbox: {blue_inbox:?}"
        );
        // A retried reply replays instead of sending twice.
        let counts = delivery_counts(&cx).await;
        let replayed: Value = serde_json::from_str(
            &reply_message(
                &ctx,
                backend.clone(),
                api_change,
                "GreenCastle".to_string(),
                "Bumped.".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some("xq-reply".to_string()),
            )
            .await
            .expect("replayed reply"),
        )
        .expect("replay JSON");
        assert_eq!(replayed["id"], reply["id"]);
        assert_eq!(replayed["idempotent_replay"], true);
        assert_eq!(delivery_counts(&cx).await, counts);

        // No placeholder in either project.
        assert_eq!(
            agent_names(&ctx, &frontend).await,
            vec!["BlueLake", "RedPeak"]
        );
        assert_eq!(agent_names(&ctx, &backend).await, vec!["GreenCastle"]);

        // One message lives in one project: local + qualified is refused.
        let counts = delivery_counts(&cx).await;
        let err = send(vec![qualified.as_str(), "RedPeak"], "mixed", None, None)
            .await
            .expect_err("recipients in two projects");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
            Some("INVALID_ARGUMENT"),
            "{err:?}"
        );
        // A qualifier naming the sender's own project is an ordinary local send.
        let frontend_slug = frontend_row["slug"].as_str().expect("slug").to_string();
        let self_qualified = format!("RedPeak@{frontend_slug}");
        send(vec![self_qualified.as_str()], "local", None, None)
            .await
            .expect("self-project qualifier is local");
        assert_eq!(delivery_counts(&cx).await.0, counts.0 + 1);

        // A blocked link refuses outright.
        respond_contact(
            &ctx,
            backend.clone(),
            "GreenCastle".to_string(),
            "BlueLake".to_string(),
            Some(frontend.clone()),
            false,
            None,
        )
        .await
        .expect("block the link");
        let counts = delivery_counts(&cx).await;
        let err = send(vec![qualified.as_str()], "after block", None, None)
            .await
            .expect_err("blocked link");
        assert_eq!(
            mcp_agent_mail_tools::tool_util::tool_error_code(&err),
            Some("CONTACT_BLOCKED"),
            "{err:?}"
        );
        assert_eq!(delivery_counts(&cx).await, counts);
        assert_eq!(
            peek_inbox(&ctx, &backend, "GreenCastle").await.len(),
            inbox.len()
        );
    });
}
