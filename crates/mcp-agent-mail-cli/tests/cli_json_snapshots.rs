#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::io::Write;

use serde_json::Value;

fn am_bin() -> PathBuf {
    // Cargo sets this for integration tests.
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn repo_root() -> PathBuf {
    // crates/mcp-agent-mail-cli -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-cli")
        .to_path_buf()
}

fn fixtures_dir() -> PathBuf {
    repo_root().join("tests/fixtures/cli_json")
}

fn artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/cli/json")
}

fn write_fixture(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture dir");
    }
    std::fs::write(path, contents).expect("write fixture");
}

fn read_fixture(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn write_artifact(case: &str, contents: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    let path = dir.join(format!("{case}.json"));
    std::fs::write(&path, contents).expect("write artifact");
    eprintln!("json snapshot mismatch saved to {}", path.display());
}

fn unified_diff(expected: &str, actual: &str) -> String {
    let diff = similar::TextDiff::from_lines(expected, actual);
    diff.unified_diff().header("expected", "actual").to_string()
}

fn canonicalize_json(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for k in keys {
                if let Some(child) = map.get(k) {
                    out.insert(k.clone(), canonicalize_json(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

fn normalize_json(v: Value, tmp_root: &Path) -> Value {
    let tmp = tmp_root.to_string_lossy().to_string();
    fn walk(v: Value, tmp: &str) -> Value {
        match v {
            Value::String(s) => Value::String(s.replace(tmp, "<TMP_ROOT>")),
            Value::Array(arr) => Value::Array(arr.into_iter().map(|x| walk(x, tmp)).collect()),
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, val) in map {
                    out.insert(k, walk(val, tmp));
                }
                Value::Object(out)
            }
            other => other,
        }
    }

    walk(canonicalize_json(&v), &tmp)
}

#[derive(Debug)]
struct TestEnv {
    tmp: tempfile::TempDir,
    db_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("mailbox.sqlite3");
        Self { tmp, db_path }
    }

    fn database_url(&self) -> String {
        format!("sqlite:///{}", self.db_path.display())
    }

    fn storage_root(&self) -> PathBuf {
        self.tmp.path().join("storage_root")
    }

    fn base_env(&self) -> Vec<(String, String)> {
        vec![
            ("DATABASE_URL".to_string(), self.database_url()),
            ("STORAGE_ROOT".to_string(), self.storage_root().display().to_string()),
            // Force server tool calls (products) to fail fast so we exercise local fallbacks.
            ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
            ("HTTP_PORT".to_string(), "1".to_string()),
            ("HTTP_PATH".to_string(), "/mcp/".to_string()),
        ]
    }
}

fn seed_cli_json_db(db_path: &Path, root: &Path) -> (String, String) {
    use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

    let created_at_us = 1_704_067_200_000_000i64; // 2024-01-01T00:00:00Z

    let proj_alpha_dir = root.join("proj_alpha");
    std::fs::create_dir_all(&proj_alpha_dir).unwrap();
    let proj_beta_dir = root.join("proj_beta");
    std::fs::create_dir_all(&proj_beta_dir).unwrap();

    let proj_alpha_key = proj_alpha_dir.canonicalize().unwrap().display().to_string();
    let proj_beta_key = proj_beta_dir.canonicalize().unwrap().display().to_string();

    let conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path.display().to_string())
        .expect("open sqlite db");
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
        .expect("init schema");

    // Projects
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(1),
            SqlValue::Text("proj-alpha".to_string()),
            SqlValue::Text(proj_alpha_key.clone()),
            SqlValue::BigInt(created_at_us),
        ],
    )
    .unwrap();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(2),
            SqlValue::Text("proj-beta".to_string()),
            SqlValue::Text(proj_beta_key.clone()),
            SqlValue::BigInt(created_at_us),
        ],
    )
    .unwrap();

    // Agents: same recipient name across both projects (legacy semantics).
    let agent_insert = "INSERT INTO agents (\
            id, project_id, name, program, model, task_description, \
            inception_ts, last_active_ts, attachments_policy, contact_policy\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

    conn.execute_sync(
        agent_insert,
        &[
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::Text("GreenCastle".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(0),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .unwrap();
    conn.execute_sync(
        agent_insert,
        &[
            SqlValue::BigInt(2),
            SqlValue::BigInt(2),
            SqlValue::Text("GreenCastle".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(0),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .unwrap();
    // Senders
    conn.execute_sync(
        agent_insert,
        &[
            SqlValue::BigInt(3),
            SqlValue::BigInt(1),
            SqlValue::Text("PurpleBear".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(0),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .unwrap();
    conn.execute_sync(
        agent_insert,
        &[
            SqlValue::BigInt(4),
            SqlValue::BigInt(2),
            SqlValue::Text("OrangeFish".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text("test".to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(0),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .unwrap();

    // Product + links
    conn.execute_sync(
        "INSERT INTO products (id, product_uid, name, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(1),
            SqlValue::Text("abc123".to_string()),
            SqlValue::Text("Test Product".to_string()),
            SqlValue::BigInt(created_at_us),
        ],
    )
    .unwrap();
    conn.execute_sync(
        "INSERT INTO product_project_links (id, product_id, project_id, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::BigInt(created_at_us),
        ],
    )
    .unwrap();
    conn.execute_sync(
        "INSERT INTO product_project_links (id, product_id, project_id, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(2),
            SqlValue::BigInt(1),
            SqlValue::BigInt(2),
            SqlValue::BigInt(created_at_us),
        ],
    )
    .unwrap();

    // Messages (FTS triggers populate fts_messages)
    let msg_insert = "INSERT INTO messages (\
            id, project_id, sender_id, thread_id, subject, body_md, importance, \
            ack_required, created_ts, attachments\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    conn.execute_sync(
        msg_insert,
        &[
            SqlValue::BigInt(10),
            SqlValue::BigInt(1),
            SqlValue::BigInt(3),
            SqlValue::Null,
            SqlValue::Text("Unicorn alpha".to_string()),
            SqlValue::Text("body alpha".to_string()),
            SqlValue::Text("high".to_string()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(created_at_us + 10),
            SqlValue::Text("[]".to_string()),
        ],
    )
    .unwrap();
    conn.execute_sync(
        msg_insert,
        &[
            SqlValue::BigInt(20),
            SqlValue::BigInt(2),
            SqlValue::BigInt(4),
            SqlValue::Null,
            SqlValue::Text("Beta ping".to_string()),
            SqlValue::Text("body beta".to_string()),
            SqlValue::Text("normal".to_string()),
            SqlValue::BigInt(0),
            SqlValue::BigInt(created_at_us + 20),
            SqlValue::Text("[]".to_string()),
        ],
    )
    .unwrap();

    // Recipients (inbox)
    let recip_insert =
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)";
    conn.execute_sync(
        recip_insert,
        &[
            SqlValue::BigInt(10),
            SqlValue::BigInt(1),
            SqlValue::Text("to".to_string()),
        ],
    )
    .unwrap();
    conn.execute_sync(
        recip_insert,
        &[
            SqlValue::BigInt(20),
            SqlValue::BigInt(2),
            SqlValue::Text("to".to_string()),
        ],
    )
    .unwrap();

    ("abc123".to_string(), "GreenCastle".to_string())
}

fn seed_archive_fixture(root: &Path) {
    use zip::write::FileOptions;

    let archive_dir = root.join("archived_mailbox_states");
    std::fs::create_dir_all(&archive_dir).unwrap();

    let zip_path = archive_dir.join("fixture.zip");
    let f = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(f);

    // Keep zip metadata deterministic-ish: store only a small file.
    let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("metadata.json", opts).unwrap();
    zip.write_all(
        br#"{
  "created_at": "2024-01-01T00:00:00Z",
  "scrub_preset": "archive",
  "projects_requested": ["all"]
}
"#,
    )
    .unwrap();
    zip.finish().unwrap();
}

fn run_json_cmd(
    env: &TestEnv,
    cwd: Option<&Path>,
    args: &[&str],
) -> (std::process::ExitStatus, String, String) {
    let mut cmd = Command::new(am_bin());
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env.base_env() {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn am");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn assert_json_snapshot(env: &TestEnv, case: &str, cwd: Option<&Path>, args: &[&str]) {
    let (status, stdout, stderr) = run_json_cmd(env, cwd, args);
    assert!(
        status.success(),
        "expected success for {case} args={args:?}, got status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status.code()
    );

    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "expected valid JSON for {case} args={args:?}, got parse error: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    });

    let normalized = normalize_json(value, env.tmp.path());
    let actual = format!("{}\n", serde_json::to_string_pretty(&normalized).unwrap());

    let fixture_path = fixtures_dir().join(format!("{case}.json"));
    let update = std::env::var("UPDATE_CLI_JSON_SNAPSHOTS")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();

    match read_fixture(&fixture_path) {
        Some(expected_raw) => {
            if expected_raw == actual {
                return;
            }
            if update {
                write_fixture(&fixture_path, &actual);
                return;
            }
            write_artifact(case, &actual);
            let diff = unified_diff(&expected_raw, &actual);
            panic!(
                "json snapshot mismatch for {case} ({args:?})\n\
                 Hint: set UPDATE_CLI_JSON_SNAPSHOTS=1 to update fixtures\n\n{diff}"
            );
        }
        None => {
            if update {
                write_fixture(&fixture_path, &actual);
                return;
            }
            write_artifact(case, &actual);
            panic!(
                "missing json fixture {path}\n\
                 Hint: generate fixtures with UPDATE_CLI_JSON_SNAPSHOTS=1",
                path = fixture_path.display()
            );
        }
    }
}

#[test]
fn cli_json_snapshots() {
    // br-2ei.5.7.4: JSON output stability
    let env = TestEnv::new();
    let (product_key, agent_name) = seed_cli_json_db(&env.db_path, env.tmp.path());
    seed_archive_fixture(env.tmp.path());

    let cases: Vec<(&str, Option<&Path>, Vec<String>)> = vec![
        // Commands with explicit --json in legacy spec:
        ("doctor_check", None, vec!["doctor", "check", "--json"].into_iter().map(String::from).collect()),
        ("doctor_backups_empty", None, vec!["doctor", "backups", "--json"].into_iter().map(String::from).collect()),
        ("list_projects", None, vec!["list-projects", "--include-agents", "--json"].into_iter().map(String::from).collect()),
        // archive list uses detect_project_root(), so run from tmp root to avoid polluting repo.
        ("archive_list", Some(env.tmp.path()), vec!["archive", "list", "--json"].into_iter().map(String::from).collect()),
        // Products JSON flags are extra, but stable and useful for automation.
        ("products_status", None, vec!["products", "status", &product_key, "--json"].into_iter().map(String::from).collect()),
        ("products_search", None, vec!["products", "search", &product_key, "Unicorn", "--json"].into_iter().map(String::from).collect()),
        ("products_inbox", None, vec!["products", "inbox", &product_key, &agent_name, "--json"].into_iter().map(String::from).collect()),
    ];

    for (case, cwd, args) in cases {
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_json_snapshot(&env, case, cwd, &args_ref);
    }
}
