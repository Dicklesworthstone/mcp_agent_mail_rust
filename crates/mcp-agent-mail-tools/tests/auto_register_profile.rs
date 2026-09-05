//! GH#301: a `send_message` to an unregistered same-project recipient
//! auto-registers a placeholder agent (when `MESSAGING_AUTO_REGISTER_RECIPIENTS`
//! is on and the registration proof gate is off). That placeholder must exist
//! in the Git archive too, or DB and archive agent inventories drift by one and
//! a reconstruct cannot recreate it.

#![recursion_limit = "256"]

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{ensure_project, register_agent, send_message};
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
