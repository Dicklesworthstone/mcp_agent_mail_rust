//! Real handshake interleaving: the recipient blocks after the macro has read
//! its pending request but before the automatic approval transaction begins.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::prelude::{McpContext, McpResult};
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

struct RequestPause {
    project: String,
    entered: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

static REQUEST_PAUSE: Mutex<Option<RequestPause>> = Mutex::new(None);

/// Compiled only into unit tests. Other requests never consume this gate.
pub(super) fn pause_after_request(project: &str) {
    let pause = {
        let mut slot = REQUEST_PAUSE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.as_ref().is_some_and(|pause| pause.project == project) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.entered.send(()).expect("report real request completion");
        pause.resume.recv_timeout(Duration::from_secs(10)).expect("resume handshake");
    }
}

struct PausedHandshake {
    project: String,
    entered: mpsc::Receiver<()>,
    resume: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<McpResult<String>>>,
}

impl PausedHandshake {
    fn start(source: String, target: String) -> Self {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let mut owner = Self {
            project: source.clone(),
            entered: entered_rx,
            resume: Some(resume_tx),
            worker: None,
        };
        {
            let mut slot = REQUEST_PAUSE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(slot.is_none(), "another request already owns the pause gate");
            *slot = Some(RequestPause {
                project: source.clone(),
                entered: entered_tx,
                resume: resume_rx,
            });
        }
        owner.worker = Some(std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread().build().expect("handshake runtime");
            runtime.block_on(async {
                let ctx = McpContext::new(Cx::current().expect("handshake context"), 2);
                handshake(&ctx, &source, &target).await
            })
        }));
        owner
    }

    fn wait_for_request(&self) {
        self.entered.recv_timeout(Duration::from_secs(10)).expect("macro read pending request");
    }

    fn release(&mut self) {
        let mut slot = REQUEST_PAUSE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.as_ref().is_some_and(|pause| pause.project == self.project) {
            slot.take();
        }
        drop(slot);
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }

    fn finish(mut self) -> McpResult<String> {
        self.release();
        self.worker.take().unwrap().join().expect("handshake worker exits")
    }
}

impl Drop for PausedHandshake {
    fn drop(&mut self) {
        // An assertion failure must not leave a worker parked or let it use
        // restored process configuration after this fixture unwinds.
        self.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn handshake(ctx: &McpContext, source: &str, target: &str) -> McpResult<String> {
    super::macro_contact_handshake(
        ctx,
        source.to_string(),
        Some("BlueLake".to_string()),
        Some("GreenCastle".to_string()),
        None,
        None,
        Some(target.to_string()),
        Some("concurrent contact decision".to_string()),
        Some(true),
        Some(3600),
        Some("Welcome after approval".to_string()),
        Some("Only a permitted handshake may deliver this.".to_string()),
        None,
        Some(false),
        None,
        None,
        None,
        None,
    )
    .await
}

async fn register_actor(ctx: &McpContext, project: &str, name: &str) -> String {
    let project = crate::ensure_project(ctx, project.to_string(), None).await.expect("project");
    let project: Value = serde_json::from_str(&project).unwrap();
    let key = project["human_key"].as_str().expect("canonical project key").to_string();
    crate::register_agent(
        ctx, key.clone(), "test".to_string(), "test".to_string(), Some(name.to_string()),
        None, None, None, None, None, None,
    )
    .await
    .expect("agent");
    crate::contacts::set_contact_policy(ctx, key.clone(), name.to_string(), "open".to_string())
        .await
        .expect("open policy still respects an explicit link block");
    key
}

async fn state(ctx: &McpContext, source: &str) -> Value {
    let pool = crate::tool_util::get_db_pool_for_request(ctx).expect("state pool");
    let project = crate::tool_util::resolve_existing_project(ctx, &pool, source)
        .await
        .expect("existing source");
    let sender = mcp_agent_mail_db::queries::get_agent(
        ctx.cx(), &pool, project.id.unwrap(), "BlueLake",
    )
    .await
    .into_result()
    .expect("source agent");
    let (outgoing, incoming) = mcp_agent_mail_db::queries::list_contacts(
        ctx.cx(), &pool, project.id.unwrap(), sender.id.unwrap(),
    )
    .await
    .into_result()
    .expect("both contact directions");
    let conn = pool.acquire(ctx.cx()).await.into_result().expect("state checkout");
    let rows = conn.query_sync(
        "SELECT (SELECT COUNT(*) FROM messages) AS messages, \
         (SELECT COUNT(*) FROM message_recipients) AS recipients",
        &[],
    ).expect("delivery counts");
    json!({
        "outgoing": outgoing,
        "incoming": incoming,
        "messages": rows[0].get_named::<i64>("messages").unwrap(),
        "recipients": rows[0].get_named::<i64>("recipients").unwrap(),
    })
}

fn archive_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).expect("archive directory") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_none_or(|name| name != ".git") {
                    visit(&path, files);
                }
            } else {
                files.insert(path.clone(), std::fs::read(&path).expect("archive file"));
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(&root.join("projects"), &mut files);
    files
}

#[test]
#[allow(clippy::too_many_lines)]
fn recipient_block_between_request_and_auto_approval_is_preserved() {
    for cross_project in [false, true] {
        let dir = tempfile::tempdir().expect("isolated mailbox");
        let database_url = format!("sqlite://{}", dir.path().join("mail.sqlite3").display());
        let storage_root = dir.path().join("archive");
        let source = dir.path().join("source");
        let target = if cross_project { dir.path().join("target") } else { source.clone() };
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", storage_root.to_str().unwrap()),
                ("MESSAGING_FAIL_CLOSED_SEND_PROFILE", "0"),
                ("CONTACT_ENFORCEMENT_ENABLED", "1"),
                ("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1"),
            ],
            || {
                Config::reset_cached();
                let runtime = RuntimeBuilder::current_thread().build().expect("fixture runtime");
                runtime.block_on(async {
                    let ctx = McpContext::new(Cx::current().expect("fixture context"), 1);
                    let source = register_actor(&ctx, source.to_str().unwrap(), "BlueLake").await;
                    let target = register_actor(&ctx, target.to_str().unwrap(), "GreenCastle").await;
                    crate::contacts::request_contact_with_intro(
                        &ctx, target.clone(), "GreenCastle".to_string(), "BlueLake".to_string(),
                        Some(source.clone()), Some("independent reverse link".to_string()),
                        Some(1800), Some(false), None, None, None,
                        crate::contacts::ContactIntro::Deferred,
                    )
                    .await
                    .expect("seed the reverse direction without mail");

                    let pending = PausedHandshake::start(source.clone(), target.clone());
                    pending.wait_for_request();
                    crate::contacts::respond_contact(
                        &ctx, target.clone(), "GreenCastle".to_string(), "BlueLake".to_string(),
                        Some(source.clone()), false, None,
                    )
                    .await
                    .expect("recipient blocks after macro read pending");
                    mcp_agent_mail_storage::wbq_flush();
                    let blocked = state(&ctx, &source).await;
                    assert_eq!(blocked["outgoing"][0]["status"], "blocked");
                    assert_eq!(blocked["incoming"][0]["status"], "pending");
                    let files = archive_files(&storage_root);

                    let response = pending.finish().expect("macro reports the winning block");
                    let response: Value = serde_json::from_str(&response).unwrap();
                    assert_eq!(response["request"]["status"], "pending", "the raced snapshot");
                    assert!(response["response"].is_null());
                    assert!(response["welcome_message"].is_null());
                    assert_eq!(response["welcome_skipped_reason"], super::WELCOME_SKIPPED_CONTACT_BLOCKED);
                    mcp_agent_mail_storage::wbq_flush();
                    assert_eq!(state(&ctx, &source).await, blocked, "all link metadata and deliveries");
                    assert_eq!(archive_files(&storage_root), files, "no notice or welcome archive");

                    let retry: Value = serde_json::from_str(
                        &handshake(&ctx, &source, &target).await.expect("already-blocked handshake"),
                    ).unwrap();
                    assert_eq!(retry["request"]["status"], "blocked");
                    assert!(retry["response"].is_null());
                    assert!(retry["welcome_message"].is_null());
                    assert_eq!(state(&ctx, &source).await, blocked);

                    let approved = crate::contacts::respond_contact(
                        &ctx, target.clone(), "GreenCastle".to_string(), "BlueLake".to_string(),
                        Some(source.clone()), true, None,
                    )
                    .await
                    .expect("explicit recipient approval still unblocks");
                    assert_eq!(serde_json::from_str::<Value>(&approved).unwrap()["approved"], true);
                    let accepted: Value = serde_json::from_str(
                        &handshake(&ctx, &source, &target).await.expect("approved handshake"),
                    ).unwrap();
                    assert_eq!(accepted["response"]["approved"], true);
                    assert!(!accepted["welcome_message"].is_null());
                    let after = state(&ctx, &source).await;
                    assert_eq!(after["incoming"], blocked["incoming"], "reverse link is independent");
                    assert_eq!(after["messages"].as_i64(), blocked["messages"].as_i64().map(|n| n + 2));
                    assert_eq!(after["recipients"].as_i64(), blocked["recipients"].as_i64().map(|n| n + 2));
                    mcp_agent_mail_storage::wbq_flush();
                });
                Config::reset_cached();
            },
        );
    }
}
