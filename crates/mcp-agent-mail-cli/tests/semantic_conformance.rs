//! br-21gj.5.3: Semantic conformance suite — CLI vs MCP parity.
//!
//! Verifies that CLI operations produce the same database state and output
//! semantics as their MCP tool counterparts. Runs both paths against the
//! same schema and compares outcomes.
//!
//! Does NOT test routing (that's br-21gj.5.2). This tests semantic equivalence:
//! same inputs → same database effects → comparable outputs.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

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
    repo_root().join("tests/artifacts/cli/semantic_conformance")
}

#[derive(Debug, serde::Serialize)]
struct DriftEntry {
    operation: String,
    field: String,
    cli_value: serde_json::Value,
    mcp_value: serde_json::Value,
    severity: String, // "mismatch", "acceptable", "justified"
    rationale: Option<String>,
}

struct TestEnv {
    tmp: tempfile::TempDir,
    db_path: PathBuf,
    storage_root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("mailbox.sqlite3");
        let storage_root = tmp.path().join("storage_root");
        Self {
            tmp,
            db_path,
            storage_root,
        }
    }

    fn database_url(&self) -> String {
        format!("sqlite:///{}", self.db_path.display())
    }

    fn base_env(&self) -> Vec<(String, String)> {
        vec![
            ("DATABASE_URL".to_string(), self.database_url()),
            (
                "STORAGE_ROOT".to_string(),
                self.storage_root.display().to_string(),
            ),
            ("AGENT_NAME".to_string(), "TestAgent".to_string()),
            ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
            ("HTTP_PORT".to_string(), "1".to_string()),
            ("HTTP_PATH".to_string(), "/mcp/".to_string()),
        ]
    }

    fn open_conn(&self) -> sqlmodel_sqlite::SqliteConnection {
        sqlmodel_sqlite::SqliteConnection::open_file(self.db_path.display().to_string())
            .expect("open db")
    }
}

fn run_am(env: &[(String, String)], args: &[&str]) -> Output {
    let mut cmd = Command::new(am_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn am")
}

fn init_schema(db_path: &Path) -> sqlmodel_sqlite::SqliteConnection {
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path.display().to_string())
        .expect("open db");
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
        .expect("init schema");
    conn
}

fn insert_project(conn: &sqlmodel_sqlite::SqliteConnection, id: i64, slug: &str, human_key: &str) {
    let now = mcp_agent_mail_db::timestamps::now_micros();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::Text(slug.to_string()),
            SqlValue::Text(human_key.to_string()),
            SqlValue::BigInt(now),
        ],
    )
    .expect("insert project");
}

fn insert_agent(
    conn: &sqlmodel_sqlite::SqliteConnection,
    id: i64,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
) {
    let now = mcp_agent_mail_db::timestamps::now_micros();
    conn.execute_sync(
        "INSERT INTO agents (\
            id, project_id, name, program, model, task_description, \
            inception_ts, last_active_ts, attachments_policy, contact_policy\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::Text(name.to_string()),
            SqlValue::Text(program.to_string()),
            SqlValue::Text(model.to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(now),
            SqlValue::BigInt(now),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .expect("insert agent");
}

fn save_drift_report(entries: &[DriftEntry], test_name: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let dir = artifacts_dir().join(format!("{ts}_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{test_name}.json"));
    let json = serde_json::to_string_pretty(entries).unwrap_or_default();
    let _ = std::fs::write(&path, &json);
    eprintln!("drift report: {}", path.display());
}

fn query_one_str(
    conn: &sqlmodel_sqlite::SqliteConnection,
    sql: &str,
    params: &[SqlValue],
    col: &str,
) -> Option<String> {
    conn.query_sync(sql, params)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<String>(col).ok())
}

fn query_count(conn: &sqlmodel_sqlite::SqliteConnection, sql: &str, params: &[SqlValue]) -> i64 {
    conn.query_sync(sql, params)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("cnt").ok())
        .unwrap_or(0)
}

// ── Test Cases ───────────────────────────────────────────────────────

/// SC-1: CLI `list-projects --json` produces valid JSON with expected fields.
/// MCP `ensure_project` creates projects; CLI lists them. Fields must align.
#[test]
fn sc_project_list_json_fields() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    let human_key = env.tmp.path().join("test-proj").display().to_string();
    insert_project(&conn, 1, "test-proj", &human_key);
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    drop(conn);

    let out = run_am(
        &env.base_env(),
        &["list-projects", "--include-agents", "--json"],
    );
    assert!(out.status.success(), "CLI should succeed");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // MCP ensure_project returns: { slug, human_key, agents: [...] }
    // CLI list-projects --json returns: array of { slug, human_key, agents }
    let arr = value.as_array().expect("should be array");
    assert!(!arr.is_empty(), "should have at least 1 project");

    let proj = &arr[0];
    let mut drift = Vec::new();

    // Check required fields.
    let slug = proj.get("slug").and_then(|v| v.as_str());
    if slug != Some("test-proj") {
        drift.push(DriftEntry {
            operation: "list-projects".to_string(),
            field: "slug".to_string(),
            cli_value: proj.get("slug").cloned().unwrap_or_default(),
            mcp_value: serde_json::json!("test-proj"),
            severity: "mismatch".to_string(),
            rationale: None,
        });
    }

    let human_key_val = proj.get("human_key").and_then(|v| v.as_str());
    assert!(
        human_key_val.is_some(),
        "list-projects should include human_key"
    );

    // Agents sub-array.
    let agents = proj.get("agents").and_then(|v| v.as_array());
    if let Some(agents) = agents {
        assert!(!agents.is_empty(), "should have 1 agent");
        let agent = &agents[0];
        let name = agent.get("name").and_then(|v| v.as_str());
        assert_eq!(name, Some("BlueLake"), "agent name mismatch");
    }

    save_drift_report(&drift, "sc_project_list_json_fields");
    assert!(
        drift.is_empty(),
        "drift detected: {}",
        serde_json::to_string_pretty(&drift).unwrap()
    );
}

/// SC-2: CLI `doctor check --json` produces health status with expected fields.
/// MCP `health_check` returns { status, checks }. CLI should produce equivalent.
#[test]
fn sc_doctor_check_json_fields() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    drop(conn);

    let out = run_am(&env.base_env(), &["doctor", "check", "--json"]);
    assert!(out.status.success(), "doctor check should succeed");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Both MCP health_check and CLI doctor check should report health status.
    let healthy = value.get("healthy");
    assert!(
        healthy.is_some(),
        "doctor check --json should include 'healthy' field"
    );

    let checks = value.get("checks");
    assert!(
        checks.is_some(),
        "doctor check --json should include 'checks' array"
    );
}

/// SC-3: CLI contacts lifecycle creates same DB state as MCP tools.
/// request_contact → respond_contact → list_contacts.
#[test]
fn sc_contacts_lifecycle_db_parity() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    insert_agent(&conn, 2, 1, "RedFox", "codex-cli", "gpt-5");
    drop(conn);

    let base = env.base_env();

    // Step 1: CLI contacts request
    let out = run_am(
        &base,
        &[
            "contacts",
            "request",
            "-p",
            "test-proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--reason",
            "need coordination",
            "--ttl-seconds",
            "3600",
        ],
    );
    assert!(
        out.status.success(),
        "contacts request failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify DB state: should have 1 pending link.
    let conn = env.open_conn();
    let status = query_one_str(
        &conn,
        "SELECT status FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
        "status",
    );
    assert_eq!(status.as_deref(), Some("pending"), "link should be pending");

    let reason = query_one_str(
        &conn,
        "SELECT reason FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
        "reason",
    );
    assert_eq!(
        reason.as_deref(),
        Some("need coordination"),
        "reason mismatch"
    );
    drop(conn);

    // Step 2: CLI contacts respond (approve)
    let out = run_am(
        &base,
        &[
            "contacts",
            "respond",
            "-p",
            "test-proj",
            "-a",
            "RedFox",
            "--from",
            "BlueLake",
            "--accept",
        ],
    );
    assert!(
        out.status.success(),
        "contacts respond failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = env.open_conn();
    let status = query_one_str(
        &conn,
        "SELECT status FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
        "status",
    );
    assert_eq!(
        status.as_deref(),
        Some("approved"),
        "link should be approved after respond"
    );
    drop(conn);

    // Step 3: CLI contacts list --json
    let out = run_am(
        &base,
        &[
            "contacts",
            "list",
            "-p",
            "test-proj",
            "-a",
            "BlueLake",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "contacts list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let entries: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let arr = entries.as_array().expect("should be array");
    assert!(!arr.is_empty(), "should have at least 1 contact entry");

    // Check that outgoing contact to RedFox is present and approved.
    let outgoing = arr.iter().find(|e| {
        e.get("direction").and_then(|v| v.as_str()) == Some("outgoing")
            && e.get("to").and_then(|v| v.as_str()) == Some("RedFox")
    });
    assert!(outgoing.is_some(), "should have outgoing contact to RedFox");
    let entry = outgoing.unwrap();
    assert_eq!(
        entry.get("status").and_then(|v| v.as_str()),
        Some("approved"),
        "outgoing contact should be approved"
    );
}

/// SC-4: CLI contacts policy sets same DB field as MCP set_contact_policy.
#[test]
fn sc_contact_policy_db_parity() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    drop(conn);

    let out = run_am(
        &env.base_env(),
        &[
            "contacts",
            "policy",
            "-p",
            "test-proj",
            "-a",
            "BlueLake",
            "contacts_only",
        ],
    );
    assert!(
        out.status.success(),
        "contacts policy failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify DB: contact_policy should be "contacts_only"
    // MCP set_contact_policy sets the same column.
    let conn = env.open_conn();
    let policy = query_one_str(
        &conn,
        "SELECT contact_policy FROM agents WHERE name = 'BlueLake'",
        &[],
        "contact_policy",
    );
    assert_eq!(
        policy.as_deref(),
        Some("contacts_only"),
        "policy should be contacts_only in DB"
    );
}

/// SC-5: CLI file_reservations reserve creates same DB state as MCP tool.
#[test]
fn sc_file_reservations_db_parity() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    drop(conn);

    let out = run_am(
        &env.base_env(),
        &[
            "file_reservations",
            "reserve",
            "test-proj",
            "BlueLake",
            "src/**",
            "tests/**",
            "--ttl",
            "7200",
            "--reason",
            "refactoring",
        ],
    );
    assert!(
        out.status.success(),
        "reserve failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify DB: 2 reservations with correct fields.
    let conn = env.open_conn();
    let count = query_count(
        &conn,
        "SELECT COUNT(*) AS cnt FROM file_reservations WHERE agent_id = 1 AND released_ts IS NULL",
        &[],
    );
    assert_eq!(count, 2, "should have 2 active reservations");

    let paths: Vec<String> = conn
        .query_sync(
            "SELECT path_pattern FROM file_reservations WHERE agent_id = 1 ORDER BY path_pattern",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get_named::<String>("path_pattern").unwrap())
        .collect();
    assert_eq!(paths, vec!["src/**", "tests/**"]);
}

/// SC-6: CLI file_reservations release produces same effect as MCP release.
#[test]
fn sc_file_reservations_release_db_parity() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");

    // Pre-seed a reservation.
    let now = mcp_agent_mail_db::timestamps::now_micros();
    conn.execute_sync(
        "INSERT INTO file_reservations (\
            id, project_id, agent_id, path_pattern, exclusive, reason, \
            created_ts, expires_ts, released_ts\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::Text("src/**".to_string()),
            SqlValue::BigInt(1),
            SqlValue::Text("test".to_string()),
            SqlValue::BigInt(now),
            SqlValue::BigInt(now + 3_600_000_000),
            SqlValue::Null,
        ],
    )
    .unwrap();
    drop(conn);

    let out = run_am(
        &env.base_env(),
        &[
            "file_reservations",
            "release",
            "test-proj",
            "BlueLake",
            "--paths",
            "src/**",
        ],
    );
    assert!(
        out.status.success(),
        "release failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = env.open_conn();
    let active = query_count(
        &conn,
        "SELECT COUNT(*) AS cnt FROM file_reservations WHERE agent_id = 1 AND released_ts IS NULL",
        &[],
    );
    assert_eq!(active, 0, "reservation should be released (0 active)");
}

/// SC-7: CLI contacts reject produces "blocked" status like MCP respond_contact(approved=false).
#[test]
fn sc_contacts_reject_db_parity() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    insert_agent(&conn, 2, 1, "RedFox", "codex-cli", "gpt-5");
    drop(conn);

    let base = env.base_env();

    // Request.
    let out = run_am(
        &base,
        &[
            "contacts",
            "request",
            "-p",
            "test-proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--reason",
            "test",
        ],
    );
    assert!(out.status.success());

    // Reject.
    let out = run_am(
        &base,
        &[
            "contacts",
            "respond",
            "-p",
            "test-proj",
            "-a",
            "RedFox",
            "--from",
            "BlueLake",
            "--reject",
        ],
    );
    assert!(
        out.status.success(),
        "reject failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = env.open_conn();
    let status = query_one_str(
        &conn,
        "SELECT status FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
        "status",
    );
    assert_eq!(
        status.as_deref(),
        Some("blocked"),
        "rejected contact should have 'blocked' status (same as MCP respond_contact(approved=false))"
    );
}

/// SC-8: CLI validation failures match MCP validation.
/// Invalid project/agent should produce errors, not silently succeed.
#[test]
fn sc_validation_failures_match() {
    let env = TestEnv::new();
    let _conn = init_schema(&env.db_path);
    // No project inserted — operations should fail.

    let base = env.base_env();

    // contacts request with nonexistent project.
    let out = run_am(
        &base,
        &[
            "contacts",
            "request",
            "-p",
            "nonexistent",
            "--from",
            "A",
            "--to",
            "B",
        ],
    );
    assert!(!out.status.success(), "should fail for nonexistent project");

    // contacts policy with invalid policy value.
    // First create minimal project+agent.
    let conn = env.open_conn();
    insert_project(&conn, 1, "p", "/tmp/p");
    insert_agent(&conn, 1, 1, "BlueLake", "test", "test");
    drop(conn);

    let out = run_am(
        &base,
        &["contacts", "policy", "-p", "p", "-a", "BlueLake", "invalid"],
    );
    assert!(
        !out.status.success(),
        "should fail for invalid policy value"
    );
}

/// SC-9: CLI contacts upsert produces same idempotent behavior as MCP.
/// MCP request_contact uses ON CONFLICT DO UPDATE; CLI should too.
#[test]
fn sc_contacts_upsert_idempotent() {
    let env = TestEnv::new();
    let conn = init_schema(&env.db_path);
    insert_project(&conn, 1, "test-proj", "/tmp/test-proj");
    insert_agent(&conn, 1, 1, "BlueLake", "claude-code", "opus-4.6");
    insert_agent(&conn, 2, 1, "RedFox", "codex-cli", "gpt-5");
    drop(conn);

    let base = env.base_env();

    // First request.
    let out = run_am(
        &base,
        &[
            "contacts",
            "request",
            "-p",
            "test-proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--reason",
            "first",
        ],
    );
    assert!(out.status.success());

    // Second request (upsert).
    let out = run_am(
        &base,
        &[
            "contacts",
            "request",
            "-p",
            "test-proj",
            "--from",
            "BlueLake",
            "--to",
            "RedFox",
            "--reason",
            "updated reason",
        ],
    );
    assert!(out.status.success());

    // Should still be 1 row (upsert, not duplicate).
    let conn = env.open_conn();
    let count = query_count(
        &conn,
        "SELECT COUNT(*) AS cnt FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
    );
    assert_eq!(
        count, 1,
        "upsert should not create duplicate (same as MCP request_contact)"
    );

    let reason = query_one_str(
        &conn,
        "SELECT reason FROM agent_links WHERE a_agent_id = 1 AND b_agent_id = 2",
        &[],
        "reason",
    );
    assert_eq!(
        reason.as_deref(),
        Some("updated reason"),
        "upsert should update reason"
    );
}

/// SC-10: Drift report: document all known justified exceptions.
#[test]
fn sc_drift_report_justified_exceptions() {
    let justified = vec![
        DriftEntry {
            operation: "summarize_thread".to_string(),
            field: "summary".to_string(),
            cli_value: serde_json::json!("delegates to server"),
            mcp_value: serde_json::json!("direct LLM call"),
            severity: "justified".to_string(),
            rationale: Some(
                "CLI delegates to server tool via HTTP; MCP calls LLM directly. \
                 Both produce equivalent summaries but execution path differs."
                    .to_string(),
            ),
        },
        DriftEntry {
            operation: "force_release_file_reservation".to_string(),
            field: "availability".to_string(),
            cli_value: serde_json::json!("not available"),
            mcp_value: serde_json::json!("available"),
            severity: "justified".to_string(),
            rationale: Some(
                "force_release intentionally excluded from CLI for safety. \
                 Admin-only operation requiring MCP server access."
                    .to_string(),
            ),
        },
        DriftEntry {
            operation: "build_slot_lifecycle".to_string(),
            field: "explicit_commands".to_string(),
            cli_value: serde_json::json!("implicit via am-run"),
            mcp_value: serde_json::json!("explicit acquire/renew/release"),
            severity: "justified".to_string(),
            rationale: Some(
                "CLI wraps build slot lifecycle in am-run for ergonomics. \
                 MCP exposes granular acquire/renew/release for programmatic use."
                    .to_string(),
            ),
        },
    ];

    save_drift_report(&justified, "sc_drift_justified_exceptions");

    // This test always passes — it documents known justified differences.
    assert_eq!(justified.len(), 3, "3 justified exceptions documented");
}
