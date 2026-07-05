//! End-to-end tests for the optional registration proof gate wired into the
//! live registration entry points.
//!
//! These exercise the REAL tool functions (`register_agent`,
//! `create_agent_identity`, `macro_start_session`, `macro_prepare_thread`)
//! against a real SQLite-backed pool, toggling the gate through configuration
//! exactly as an operator would, and asserting:
//!
//! - disabled gate  => registration works with no proof (unchanged behavior);
//! - enabled gate + no proof   => every entry point fails closed (PROOF_REQUIRED);
//! - enabled gate + valid proof => registration succeeds through the tool and
//!   through a macro (proving macros forward the proof and cannot bypass it).

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use fastmcp::prelude::McpContext;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{
    create_agent_identity, ensure_project, macro_prepare_thread, macro_start_session,
    register_agent,
};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Capabilities `register_agent` grants by default; the proof must authorize a
/// superset of these (kept in sync with `identity::DEFAULT_AGENT_CAPABILITIES`).
const DEFAULT_CAPS: &[&str] = &[
    "send_message",
    "fetch_inbox",
    "file_reservation_paths",
    "acknowledge_message",
];

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    u64::try_from(micros)
        .unwrap_or(u64::MAX)
        .wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(0)
}

/// Run `f` serially with a fresh temp DB/storage plus any extra env overrides
/// (used to toggle the proof gate). Mirrors the harness used by the other
/// parity integration tests.
fn run_with_env<F, Fut, T>(extra: &[(&str, &str)], f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let database_url = format!("sqlite:///tmp/proof-gate-{suffix}.sqlite3");
    let storage_root = format!("/tmp/proof-gate-storage-{suffix}");
    let mut env: Vec<(&str, &str)> = vec![
        ("DATABASE_URL", database_url.as_str()),
        ("STORAGE_ROOT", storage_root.as_str()),
    ];
    env.extend_from_slice(extra);
    with_process_env_overrides_for_test(&env, || {
        Config::reset_cached();
        let cx = Cx::for_testing();
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        rt.block_on(f(cx))
    })
}

fn error_type(err: &fastmcp::McpError) -> String {
    err.data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| root.get("error"))
        .and_then(Value::as_object)
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("<no type>")
        .to_string()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Reproduce the verifier's canonical signed bytes (see
/// `mcp_agent_mail_tools::proof_gate::canonical_message`). Any external signer
/// would reproduce exactly this.
fn canonical_message(
    identity: &str,
    project_key: &str,
    program: &str,
    model: &str,
    caps: &[&str],
    issued_at: i64,
    expires_at: i64,
    nonce: &str,
) -> String {
    let mut c: Vec<String> = caps
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    c.sort();
    c.dedup();
    format!(
        "agent-mail-registration-proof:v1\n\
         identity={identity}\n\
         project_key={project_key}\n\
         program={program}\n\
         model={model}\n\
         capabilities={caps}\n\
         issued_at={issued_at}\n\
         expires_at={expires_at}\n\
         nonce={nonce}",
        caps = c.join(","),
    )
}

/// Build a valid signed proof bundle JSON string for the given registration.
#[allow(clippy::too_many_arguments)]
fn signed_proof(
    key: &SigningKey,
    identity: &str,
    project_key: &str,
    program: &str,
    model: &str,
    caps: &[&str],
    issued_at: i64,
    expires_at: i64,
    nonce: &str,
) -> String {
    let msg = canonical_message(
        identity,
        project_key,
        program,
        model,
        caps,
        issued_at,
        expires_at,
        nonce,
    );
    let sig = key.sign(msg.as_bytes());
    serde_json::json!({
        "claims": {
            "identity": identity,
            "project_key": project_key,
            "program": program,
            "model": model,
            "capabilities": caps,
            "issued_at": issued_at,
            "expires_at": expires_at,
            "nonce": nonce,
        },
        "public_key": b64(key.verifying_key().as_bytes()),
        "signature": b64(&sig.to_bytes()),
    })
    .to_string()
}

#[test]
fn disabled_gate_registers_without_proof() {
    run_with_env(&[], |cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/proof-off-{}", unique_suffix());
        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project");

        // register_agent with NO proof still works when the gate is off.
        register_agent(
            &ctx,
            project_key.clone(),
            "claude-code".to_string(),
            "opus-4.1".to_string(),
            Some("BlueLake".to_string()),
            Some("proof gate disabled".to_string()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("register_agent should succeed with gate disabled");

        // create_agent_identity with NO proof also works.
        create_agent_identity(
            &ctx,
            project_key.clone(),
            "claude-code".to_string(),
            "opus-4.1".to_string(),
            Some("GreenCastle".to_string()),
            Some("proof gate disabled".to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("create_agent_identity should succeed with gate disabled");

        // macro_start_session with NO proof also works.
        macro_start_session(
            &ctx,
            project_key.clone(),
            "claude-code".to_string(),
            "opus-4.1".to_string(),
            Some("RedStone".to_string()),
            Some("proof gate disabled".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("macro_start_session should succeed with gate disabled");
    });
}

#[test]
fn enabled_gate_blocks_every_entry_point_without_proof() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let trusted = b64(key.verifying_key().as_bytes());
    run_with_env(
        &[
            ("AM_REGISTRATION_PROOF_GATE_ENABLED", "true"),
            ("AM_REGISTRATION_PROOF_TRUSTED_KEYS", trusted.as_str()),
        ],
        |cx| async move {
            let ctx = McpContext::new(cx.clone(), 1);
            let project_key = format!("/tmp/proof-on-{}", unique_suffix());
            ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project");

            // 1. register_agent
            let err = register_agent(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("BlueLake".to_string()),
                Some("no proof".to_string()),
                None,
                None,
                None,
                None,
            )
            .await
            .expect_err("register_agent must fail closed without proof");
            assert_eq!(error_type(&err), "PROOF_REQUIRED");

            // 2. create_agent_identity
            let err = create_agent_identity(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("GreenCastle".to_string()),
                Some("no proof".to_string()),
                None,
                None,
                None,
            )
            .await
            .expect_err("create_agent_identity must fail closed without proof");
            assert_eq!(error_type(&err), "PROOF_REQUIRED");

            // 3. macro_start_session
            let err = macro_start_session(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("RedStone".to_string()),
                Some("no proof".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect_err("macro_start_session must fail closed without proof");
            assert_eq!(error_type(&err), "PROOF_REQUIRED");

            // 4. macro_prepare_thread (register_if_missing defaults to true)
            let err = macro_prepare_thread(
                &ctx,
                project_key.clone(),
                "br-1".to_string(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("AmberRiver".to_string()),
                Some("no proof".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect_err("macro_prepare_thread must fail closed without proof");
            assert_eq!(error_type(&err), "PROOF_REQUIRED");
        },
    );
}

#[test]
fn enabled_gate_allows_valid_proof_through_tool_and_macro() {
    let key = SigningKey::from_bytes(&[9u8; 32]);
    let trusted = b64(key.verifying_key().as_bytes());
    run_with_env(
        &[
            ("AM_REGISTRATION_PROOF_GATE_ENABLED", "true"),
            ("AM_REGISTRATION_PROOF_TRUSTED_KEYS", trusted.as_str()),
        ],
        |cx| async move {
            let ctx = McpContext::new(cx.clone(), 1);
            let project_key = format!("/tmp/proof-ok-{}", unique_suffix());
            ensure_project(&ctx, project_key.clone(), None)
                .await
                .expect("ensure_project");

            let now = now_unix();

            // Direct tool: valid proof for BlueLake registers.
            let proof = signed_proof(
                &key,
                "BlueLake",
                &project_key,
                "claude-code",
                "opus-4.1",
                DEFAULT_CAPS,
                now,
                now + 120,
                "nonce-tool",
            );
            register_agent(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("BlueLake".to_string()),
                Some("valid proof".to_string()),
                None,
                None,
                None,
                Some(proof),
            )
            .await
            .expect("register_agent should succeed with a valid proof");

            // Macro: valid proof forwarded through macro_start_session registers.
            let macro_proof = signed_proof(
                &key,
                "GreenCastle",
                &project_key,
                "claude-code",
                "opus-4.1",
                DEFAULT_CAPS,
                now,
                now + 120,
                "nonce-macro",
            );
            macro_start_session(
                &ctx,
                project_key.clone(),
                "claude-code".to_string(),
                "opus-4.1".to_string(),
                Some("GreenCastle".to_string()),
                Some("valid proof".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(macro_proof),
            )
            .await
            .expect("macro_start_session should succeed with a valid proof");
        },
    );
}
