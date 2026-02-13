//! br-2xz9: Native security/privacy harness for E2E gate coverage.
//!
//! Reimplements high-signal checks from tests/e2e/test_security_privacy.sh
//! using direct stdio JSON-RPC sessions against `am serve-stdio`.

#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};

const PROJECT_ALPHA: &str = "/tmp/e2e_sec_alpha";
const PROJECT_BETA: &str = "/tmp/e2e_sec_beta";

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    if let Ok(override_root) = std::env::var("AM_SECURITY_PRIVACY_ARTIFACT_DIR") {
        return PathBuf::from(override_root).join("security_privacy");
    }
    repo_root().join("tests/artifacts/cli/security_privacy")
}

#[derive(Debug)]
struct SessionRun {
    responses: Vec<Value>,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
struct CaseArtifact<'a> {
    case: &'a str,
    exit_code: Option<i32>,
    stdout: &'a str,
    stderr: &'a str,
    responses: &'a [Value],
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "native-security-privacy-harness", "version": "1.0" }
        }
    })
}

fn tool_call(id: i64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments
        }
    })
}

fn run_stdio_session(db_path: &Path, requests: &[Value]) -> SessionRun {
    let mut cmd = Command::new(am_bin());
    cmd.arg("serve-stdio")
        .env("DATABASE_URL", format!("sqlite:///{}", db_path.display()))
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn `am serve-stdio`");
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        let mut send = Vec::with_capacity(requests.len() + 1);
        send.push(initialize_request());
        send.extend_from_slice(requests);
        for req in send {
            serde_json::to_writer(&mut stdin, &req).expect("serialize request");
            stdin.write_all(b"\n").expect("write request delimiter");
        }
        stdin.flush().expect("flush child stdin");
    }

    let timeout = Duration::from_secs(15);
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("try_wait failed: {error}"),
        }
    }

    let output = child.wait_with_output().expect("wait_with_output");
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str("session timed out after 15s");
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let responses = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect::<Vec<_>>();

    SessionRun {
        responses,
        stdout,
        stderr,
        exit_code: output.status.code(),
    }
}

fn write_case_artifact(run_root: &Path, case: &str, run: &SessionRun) {
    let payload = CaseArtifact {
        case,
        exit_code: run.exit_code,
        stdout: &run.stdout,
        stderr: &run.stderr,
        responses: &run.responses,
    };
    let path = run_root.join(format!("{case}.json"));
    let content = serde_json::to_string_pretty(&payload).expect("serialize artifact");
    fs::write(path, content).expect("write artifact");
}

fn response_by_id(responses: &[Value], id: i64) -> Option<&Value> {
    responses
        .iter()
        .find(|resp| resp.get("id").and_then(Value::as_i64) == Some(id))
}

fn response_is_error(resp: &Value) -> bool {
    if resp.get("error").is_some() {
        return true;
    }
    resp.get("result")
        .and_then(|v| v.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn assert_tool_ok<'a>(responses: &'a [Value], id: i64, label: &str) -> &'a Value {
    let resp = response_by_id(responses, id)
        .unwrap_or_else(|| panic!("{label}: missing response for id={id}"));
    assert!(
        !response_is_error(resp),
        "{label}: expected success, got error response: {resp}"
    );
    resp
}

fn assert_tool_error(responses: &[Value], id: i64, label: &str) {
    let resp = response_by_id(responses, id)
        .unwrap_or_else(|| panic!("{label}: missing response for id={id}"));
    assert!(
        response_is_error(resp),
        "{label}: expected error, got success response: {resp}"
    );
}

fn response_text(resp: &Value) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse_text_json(resp: &Value) -> Value {
    let text = response_text(resp);
    serde_json::from_str(&text).unwrap_or_else(|_| Value::Null)
}

fn parse_search_results(resp: &Value) -> Vec<Value> {
    let parsed = parse_text_json(resp);
    let inner = parsed.get("result").unwrap_or(&parsed);
    inner.as_array().cloned().unwrap_or_default()
}

fn parse_inbox(resp: &Value) -> Vec<Value> {
    parse_text_json(resp)
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn native_security_privacy_gate() {
    let run_root = artifacts_dir().join(format!(
        "{}_{}",
        Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    fs::create_dir_all(&run_root).expect("create artifact root");
    eprintln!("security_privacy artifact root: {}", run_root.display());

    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("security.sqlite3");

    // Setup: projects + agents
    let setup = run_stdio_session(
        &db_path,
        &[
            tool_call(10, "ensure_project", json!({ "human_key": PROJECT_ALPHA })),
            tool_call(11, "ensure_project", json!({ "human_key": PROJECT_BETA })),
            tool_call(
                12,
                "register_agent",
                json!({ "project_key": PROJECT_ALPHA, "program": "test", "model": "test", "name": "RedFox" }),
            ),
            tool_call(
                13,
                "register_agent",
                json!({ "project_key": PROJECT_ALPHA, "program": "test", "model": "test", "name": "BlueLake" }),
            ),
            tool_call(
                14,
                "register_agent",
                json!({ "project_key": PROJECT_ALPHA, "program": "test", "model": "test", "name": "GoldPeak" }),
            ),
            tool_call(
                15,
                "register_agent",
                json!({ "project_key": PROJECT_BETA, "program": "test", "model": "test", "name": "SilverCove" }),
            ),
        ],
    );
    write_case_artifact(&run_root, "01_setup", &setup);
    for id in [10, 11, 12, 13, 14, 15] {
        let _ = assert_tool_ok(&setup.responses, id, "setup");
    }

    // Seed messages across projects.
    let seed = run_stdio_session(
        &db_path,
        &[
            tool_call(
                20,
                "send_message",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "sender_name": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Alpha secret plan",
                    "body_md": "The deployment key is sk-ant-abc123secret"
                }),
            ),
            tool_call(
                21,
                "send_message",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "sender_name": "RedFox",
                    "to": ["BlueLake"],
                    "bcc": ["GoldPeak"],
                    "subject": "BCC test message",
                    "body_md": "GoldPeak is BCC on this"
                }),
            ),
            tool_call(
                22,
                "send_message",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "sender_name": "BlueLake",
                    "to": ["GoldPeak"],
                    "subject": "Private to GoldPeak",
                    "body_md": "Only GoldPeak should see this in inbox"
                }),
            ),
            tool_call(
                23,
                "send_message",
                json!({
                    "project_key": PROJECT_BETA,
                    "sender_name": "SilverCove",
                    "to": ["SilverCove"],
                    "subject": "Beta internal note",
                    "body_md": "This message lives in project beta only"
                }),
            ),
            tool_call(
                24,
                "send_message",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "sender_name": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Hostile markdown test",
                    "body_md": "<script>alert(1)</script>\n![img](javascript:alert(1))\n[click](data:text/html,<h1>xss</h1>)"
                }),
            ),
        ],
    );
    write_case_artifact(&run_root, "02_seed_messages", &seed);
    for id in [20, 21, 22, 23, 24] {
        let _ = assert_tool_ok(&seed.responses, id, "seed");
    }

    // Case 1: project-scoped search visibility.
    let scope = run_stdio_session(
        &db_path,
        &[
            tool_call(
                100,
                "search_messages",
                json!({ "project_key": PROJECT_ALPHA, "query": "secret" }),
            ),
            tool_call(
                101,
                "search_messages",
                json!({ "project_key": PROJECT_BETA, "query": "secret" }),
            ),
            tool_call(
                102,
                "search_messages",
                json!({ "project_key": PROJECT_ALPHA, "query": "Beta internal" }),
            ),
        ],
    );
    write_case_artifact(&run_root, "03_search_scope", &scope);
    let alpha_secret =
        parse_search_results(assert_tool_ok(&scope.responses, 100, "search alpha secret"));
    let beta_secret =
        parse_search_results(assert_tool_ok(&scope.responses, 101, "search beta secret"));
    let alpha_beta = parse_search_results(assert_tool_ok(
        &scope.responses,
        102,
        "search alpha for beta content",
    ));
    assert_eq!(
        alpha_secret.len(),
        1,
        "alpha should find one secret message"
    );
    assert_eq!(beta_secret.len(), 0, "beta should not find alpha secret");
    assert_eq!(
        alpha_beta.len(),
        0,
        "alpha should not see beta message content"
    );

    // Case 2 + 3: inbox isolation and BCC privacy.
    let inboxes = run_stdio_session(
        &db_path,
        &[
            tool_call(
                200,
                "fetch_inbox",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "BlueLake", "include_bodies": true }),
            ),
            tool_call(
                201,
                "fetch_inbox",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "RedFox", "include_bodies": true }),
            ),
            tool_call(
                202,
                "fetch_inbox",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "GoldPeak", "include_bodies": true }),
            ),
        ],
    );
    write_case_artifact(&run_root, "04_inbox_isolation_bcc", &inboxes);
    let blue_msgs = parse_inbox(assert_tool_ok(&inboxes.responses, 200, "BlueLake inbox"));
    let red_msgs = parse_inbox(assert_tool_ok(&inboxes.responses, 201, "RedFox inbox"));
    let gold_msgs = parse_inbox(assert_tool_ok(&inboxes.responses, 202, "GoldPeak inbox"));
    assert!(
        blue_msgs.len() >= 3,
        "BlueLake should have >=3 messages, got {}",
        blue_msgs.len()
    );
    assert_eq!(red_msgs.len(), 0, "RedFox should have zero inbox messages");
    assert!(
        gold_msgs.len() >= 2,
        "GoldPeak should have >=2 messages (bcc + direct), got {}",
        gold_msgs.len()
    );

    let bcc_msg = blue_msgs
        .iter()
        .find(|msg| {
            msg.get("subject")
                .and_then(Value::as_str)
                .is_some_and(|subject| subject.contains("BCC test"))
        })
        .expect("BlueLake should have BCC test message");
    let recipients_view = json!({
        "to": bcc_msg.get("to").cloned().unwrap_or(Value::Array(Vec::new())),
        "cc": bcc_msg.get("cc").cloned().unwrap_or(Value::Array(Vec::new())),
        "bcc": bcc_msg.get("bcc").cloned().unwrap_or(Value::Array(Vec::new()))
    })
    .to_string();
    assert!(
        !recipients_view.contains("GoldPeak"),
        "BCC recipient leaked in recipient metadata: {recipients_view}"
    );

    // Case 4 + 5: contact policy lifecycle.
    let contacts = run_stdio_session(
        &db_path,
        &[
            tool_call(
                400,
                "set_contact_policy",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "GoldPeak", "policy": "block_all" }),
            ),
            tool_call(
                401,
                "request_contact",
                json!({ "project_key": PROJECT_ALPHA, "from_agent": "RedFox", "to_agent": "GoldPeak", "reason": "Trying to reach GoldPeak" }),
            ),
            tool_call(
                500,
                "set_contact_policy",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "BlueLake", "policy": "contacts_only" }),
            ),
            tool_call(
                501,
                "request_contact",
                json!({ "project_key": PROJECT_ALPHA, "from_agent": "GoldPeak", "to_agent": "BlueLake", "reason": "Need access" }),
            ),
            tool_call(
                502,
                "respond_contact",
                json!({ "project_key": PROJECT_ALPHA, "to_agent": "BlueLake", "from_agent": "GoldPeak", "accept": true }),
            ),
            tool_call(
                503,
                "list_contacts",
                json!({ "project_key": PROJECT_ALPHA, "agent_name": "BlueLake" }),
            ),
        ],
    );
    write_case_artifact(&run_root, "05_contact_policy", &contacts);
    let _ = assert_tool_ok(&contacts.responses, 400, "set block_all");
    assert!(
        response_by_id(&contacts.responses, 401).is_some(),
        "block_all request_contact should return a response"
    );
    for id in [500, 501, 502, 503] {
        let _ = assert_tool_ok(&contacts.responses, id, "contacts_only flow");
    }
    let contacts_text = response_text(assert_tool_ok(&contacts.responses, 503, "list contacts"));
    assert!(
        contacts_text.contains("GoldPeak"),
        "GoldPeak missing from contacts_only approval output: {contacts_text}"
    );

    // Case 6: hostile markdown is not surfaced as executable subject content.
    let hostile = run_stdio_session(
        &db_path,
        &[tool_call(
            600,
            "search_messages",
            json!({ "project_key": PROJECT_ALPHA, "query": "Hostile markdown" }),
        )],
    );
    write_case_artifact(&run_root, "06_hostile_markdown", &hostile);
    let hostile_results = parse_search_results(assert_tool_ok(
        &hostile.responses,
        600,
        "search hostile markdown",
    ));
    assert_eq!(
        hostile_results.len(),
        1,
        "expected exactly one hostile-markdown hit"
    );
    for result in &hostile_results {
        let subject = result
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            !subject.contains("<script>"),
            "subject contains raw script tag: {subject}"
        );
    }

    // Case 7: path traversal thread IDs should not leak filesystem data.
    let traversal = run_stdio_session(
        &db_path,
        &[
            tool_call(
                700,
                "summarize_thread",
                json!({ "project_key": PROJECT_ALPHA, "thread_id": "../../etc/passwd", "llm_mode": false }),
            ),
            tool_call(
                701,
                "summarize_thread",
                json!({ "project_key": PROJECT_ALPHA, "thread_id": "..\\\\..\\\\windows\\\\system32", "llm_mode": false }),
            ),
        ],
    );
    write_case_artifact(&run_root, "07_path_traversal", &traversal);
    for id in [700, 701] {
        if let Some(resp) = response_by_id(&traversal.responses, id)
            && !response_is_error(resp)
        {
            let text = response_text(resp).to_lowercase();
            assert!(
                !text.contains("root:x:"),
                "path traversal leaked passwd-like content for id={id}: {text}"
            );
            assert!(
                !text.contains("drivers\\etc\\hosts"),
                "path traversal leaked windows system path for id={id}: {text}"
            );
        }
    }

    // Case 8: oversized query strings are handled gracefully.
    let big_query = "A".repeat(10_000);
    let oversized = run_stdio_session(
        &db_path,
        &[tool_call(
            800,
            "search_messages",
            json!({ "project_key": PROJECT_ALPHA, "query": big_query }),
        )],
    );
    write_case_artifact(&run_root, "08_oversized_query", &oversized);
    assert!(
        response_by_id(&oversized.responses, 800).is_some(),
        "oversized query should yield a response"
    );

    // Case 9: secret-containing inbox message should be accessible to legitimate recipient.
    let secrets = run_stdio_session(
        &db_path,
        &[tool_call(
            900,
            "fetch_inbox",
            json!({ "project_key": PROJECT_ALPHA, "agent_name": "BlueLake", "include_bodies": true }),
        )],
    );
    write_case_artifact(&run_root, "09_secret_body", &secrets);
    let secret_msgs = parse_inbox(assert_tool_ok(
        &secrets.responses,
        900,
        "fetch inbox secrets",
    ));
    let has_secret_subject = secret_msgs.iter().any(|msg| {
        msg.get("subject")
            .and_then(Value::as_str)
            .is_some_and(|subject| subject == "Alpha secret plan")
    });
    assert!(
        has_secret_subject,
        "BlueLake inbox should include the seeded secret-plan message"
    );

    // Case 10: exclusive reservation conflict.
    let conflict = run_stdio_session(
        &db_path,
        &[
            tool_call(
                1000,
                "file_reservation_paths",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "agent_name": "RedFox",
                    "paths": ["src/critical.rs"],
                    "ttl_seconds": 3600,
                    "exclusive": true,
                    "reason": "editing critical file"
                }),
            ),
            tool_call(
                1001,
                "file_reservation_paths",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "agent_name": "BlueLake",
                    "paths": ["src/critical.rs"],
                    "ttl_seconds": 3600,
                    "exclusive": true,
                    "reason": "also want critical file"
                }),
            ),
        ],
    );
    write_case_artifact(&run_root, "10_reservation_conflict", &conflict);
    let _ = assert_tool_ok(&conflict.responses, 1000, "first reservation");
    let conflict_data = parse_text_json(assert_tool_ok(
        &conflict.responses,
        1001,
        "conflicting reservation",
    ));
    let conflict_count = conflict_data
        .get("conflicts")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(
        conflict_count, 1,
        "expected one conflict for second exclusive reservation"
    );

    // Case 11: release then re-acquire.
    let release = run_stdio_session(
        &db_path,
        &[
            tool_call(
                1100,
                "release_file_reservations",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "agent_name": "RedFox",
                    "paths": ["src/critical.rs"]
                }),
            ),
            tool_call(
                1101,
                "file_reservation_paths",
                json!({
                    "project_key": PROJECT_ALPHA,
                    "agent_name": "BlueLake",
                    "paths": ["src/critical.rs"],
                    "ttl_seconds": 3600,
                    "exclusive": true,
                    "reason": "RedFox released"
                }),
            ),
        ],
    );
    write_case_artifact(&run_root, "11_release_reacquire", &release);
    let _ = assert_tool_ok(&release.responses, 1100, "release reservation");
    let reacquire = parse_text_json(assert_tool_ok(
        &release.responses,
        1101,
        "reacquire reservation",
    ));
    let granted_count = reacquire
        .get("granted")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let post_conflict_count = reacquire
        .get("conflicts")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(granted_count, 1, "expected one granted reservation");
    assert_eq!(
        post_conflict_count, 0,
        "expected zero conflicts after release/reacquire"
    );

    // Case 12: non-existent agent fetch_inbox should error.
    let nonexistent = run_stdio_session(
        &db_path,
        &[tool_call(
            1200,
            "fetch_inbox",
            json!({ "project_key": PROJECT_ALPHA, "agent_name": "NoSuchAgent" }),
        )],
    );
    write_case_artifact(&run_root, "12_nonexistent_agent", &nonexistent);
    assert_tool_error(&nonexistent.responses, 1200, "non-existent agent inbox");
}
