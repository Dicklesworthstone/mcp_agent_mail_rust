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
    ensure_project, list_agents, macro_contact_handshake, register_agent, send_message,
};
use serde_json::Value;
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
    ];
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let out = rt.block_on(f(cx, storage_root.clone()));
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
