#![forbid(unsafe_code)]

use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash as _, Hasher as _};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use mcp_agent_mail_db::sqlmodel::Value as SqlValue;
use serde_json::{Value, json};

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

fn artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/cli/integration")
}

fn write_artifact(case: &str, args: &[&str], out: &Output) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    let path = dir.join(format!("{case}.txt"));

    let exit = out
        .status
        .code()
        .map_or_else(|| "<signal>".to_string(), |c| c.to_string());
    let body = format!(
        "args: {args:?}\nexit_code: {exit}\n\n--- stdout ---\n{stdout}\n\n--- stderr ---\n{stderr}\n",
        stdout = String::from_utf8_lossy(&out.stdout),
        stderr = String::from_utf8_lossy(&out.stderr),
    );
    std::fs::write(&path, body).expect("write artifact");
    eprintln!(
        "cli integration failure artifact saved to {}",
        path.display()
    );
}

#[derive(Debug)]
struct TestEnv {
    tmp: tempfile::TempDir,
    db_path: PathBuf,
    storage_root: PathBuf,
    home_dir: PathBuf,
    xdg_config_home: PathBuf,
    hostile_repo: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("mailbox.sqlite3");
        let storage_root = tmp.path().join("storage_root");
        let home_dir = tmp.path().join("home");
        let xdg_config_home = home_dir.join(".config");
        let hostile_repo = tmp.path().join("hostile_repo");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        std::fs::create_dir_all(&xdg_config_home).expect("create xdg config home");
        std::fs::create_dir_all(home_dir.join(".cache")).expect("create xdg cache home");
        std::fs::create_dir_all(home_dir.join(".local/share")).expect("create xdg data home");
        std::fs::create_dir_all(&hostile_repo).expect("create hostile repo");
        Self {
            tmp,
            db_path,
            storage_root,
            home_dir,
            xdg_config_home,
            hostile_repo,
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
            (
                "AM_ALLOW_EPHEMERAL_PROJECT_ROOTS".to_string(),
                "1".to_string(),
            ),
            // Guard check requires this.
            ("AGENT_NAME".to_string(), "RusticGlen".to_string()),
            // Avoid accidental network calls: force HTTP tool paths to fail fast.
            ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
            ("HTTP_PORT".to_string(), "1".to_string()),
            ("HTTP_PATH".to_string(), "/mcp/".to_string()),
        ]
    }

    fn hermetic_env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".to_string(), self.home_dir.display().to_string()),
            (
                "XDG_CONFIG_HOME".to_string(),
                self.xdg_config_home.display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".to_string(),
                self.home_dir.join(".cache").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                self.home_dir.join(".local/share").display().to_string(),
            ),
            (
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string(),
            ),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("AGENT_NAME".to_string(), "RusticGlen".to_string()),
            ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
            ("HTTP_PORT".to_string(), "1".to_string()),
            ("HTTP_PATH".to_string(), "/mcp/".to_string()),
        ]
    }

    fn isolated_env(&self) -> Vec<(String, String)> {
        let mut env = self.base_env();
        env.extend(self.hermetic_env());
        env
    }

    fn user_config_env_path(&self) -> PathBuf {
        self.xdg_config_home.join("mcp-agent-mail/config.env")
    }

    fn write_user_config_env(&self, contents: &str) {
        let path = self.user_config_env_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create user config dir");
        }
        std::fs::write(path, contents).expect("write user config env");
    }

    fn hostile_repo(&self) -> &Path {
        &self.hostile_repo
    }
}

fn run_am(
    env: &[(String, String)],
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Output {
    run_am_binary(&am_bin(), env, cwd, args, stdin)
}

fn run_am_binary(
    binary: &Path,
    env: &[(String, String)],
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Output {
    let mut cmd = Command::new(binary);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    if let Some(stdin_bytes) = stdin {
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn am");
        {
            use std::io::Write;
            let mut handle = child.stdin.take().expect("child stdin");
            handle.write_all(stdin_bytes).expect("write stdin to am");
        }
        child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("wait for {} output: {error}", binary.display()))
    } else {
        cmd.output()
            .unwrap_or_else(|error| panic!("spawn {}: {error}", binary.display()))
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct TimedProcessOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

#[allow(dead_code)]
fn run_am_with_timeout(
    env: &[(String, String)],
    cwd: Option<&Path>,
    args: &[&str],
    timeout: Duration,
) -> TimedProcessOutput {
    let mut cmd = Command::new(am_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().expect("spawn timed am");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut stderr = child.stderr.take().expect("child stderr");
    let stdout_thread = thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).expect("read child stdout");
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        stderr.read_to_string(&mut buf).expect("read child stderr");
        buf
    });

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
            Err(error) => panic!("timed am wait failed: {error}"),
        }
    }

    let exit_code = child.wait().expect("wait timed am").code();
    TimedProcessOutput {
        stdout: stdout_thread.join().expect("join stdout thread"),
        stderr: stderr_thread.join().expect("join stderr thread"),
        exit_code,
        timed_out,
    }
}

fn run_am_hermetic(env: &[(String, String)], cwd: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new(am_bin());
    cmd.env_clear();
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn hermetic am")
}

#[allow(dead_code)]
#[derive(Debug)]
struct StdioSessionRun {
    responses: Vec<Value>,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

#[allow(dead_code)]
fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "startup-recovery-crash-replay",
                "version": "1.0"
            }
        }
    })
}

#[allow(dead_code)]
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

#[allow(dead_code)]
fn run_stdio_session(env: &[(String, String)], requests: &[Value]) -> StdioSessionRun {
    let mut cmd = Command::new(am_bin());
    cmd.arg("serve-stdio")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().expect("spawn `am serve-stdio`");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut stderr = child.stderr.take().expect("child stderr");
    let stdout_thread = thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).expect("read child stdout");
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        stderr.read_to_string(&mut buf).expect("read child stderr");
        buf
    });
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        serde_json::to_writer(&mut stdin, &initialize_request()).expect("serialize initialize");
        stdin.write_all(b"\n").expect("write initialize delimiter");
        for request in requests {
            serde_json::to_writer(&mut stdin, request).expect("serialize stdio request");
            stdin.write_all(b"\n").expect("write request delimiter");
        }
        stdin.flush().expect("flush stdio requests");
    }

    let started = Instant::now();
    let timeout = Duration::from_secs(60);
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
            Err(error) => panic!("stdio wait failed: {error}"),
        }
    }

    let exit_code = child.wait().expect("wait stdio child").code();
    let stdout = stdout_thread.join().expect("join stdout thread");
    let stderr = stderr_thread.join().expect("join stderr thread");
    let responses = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();
    StdioSessionRun {
        responses,
        stdout,
        stderr,
        exit_code,
        timed_out,
    }
}

fn unused_loopback_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind unused loopback port");
    listener.local_addr().expect("local addr").port()
}

fn json_check<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("missing check {id}"))
}

fn init_cli_schema(db_path: &Path) {
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
        .expect("open sqlite db");
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
        .expect("init schema");
    conn.close_sync().expect("close initialized sqlite db");
    mcp_agent_mail_db::pool::wal_checkpoint_truncate_path(db_path)
        .expect("checkpoint initialized sqlite db");
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    kind: &'static str,
    len: Option<u64>,
    content_hash: Option<u64>,
    symlink_target: Option<PathBuf>,
    mode: Option<u32>,
    modified: Option<std::time::SystemTime>,
}

fn file_snapshot(path: &Path) -> (u64, u64) {
    let bytes =
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len() as u64, hasher.finish())
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn visit(root: &Path, current: &Path, out: &mut BTreeMap<PathBuf, TreeEntry>) {
        let mut entries: Vec<_> = std::fs::read_dir(current)
            .unwrap_or_else(|error| panic!("read_dir {}: {error}", current.display()))
            .map(|entry| entry.expect("read dir entry"))
            .collect();
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .expect("entry below snapshot root")
                .to_path_buf();
            let metadata = std::fs::symlink_metadata(&path)
                .unwrap_or_else(|error| panic!("metadata {}: {error}", path.display()));
            let file_type = metadata.file_type();
            let mode = file_mode(&metadata);
            let modified = metadata.modified().ok();

            if file_type.is_dir() {
                out.insert(
                    rel,
                    TreeEntry {
                        kind: "dir",
                        len: None,
                        content_hash: None,
                        symlink_target: None,
                        mode,
                        modified,
                    },
                );
                visit(root, &path, out);
            } else if file_type.is_file() {
                let (len, content_hash) = file_snapshot(&path);
                out.insert(
                    rel,
                    TreeEntry {
                        kind: "file",
                        len: Some(len),
                        content_hash: Some(content_hash),
                        symlink_target: None,
                        mode,
                        modified,
                    },
                );
            } else if file_type.is_symlink() {
                out.insert(
                    rel,
                    TreeEntry {
                        kind: "symlink",
                        len: None,
                        content_hash: None,
                        symlink_target: Some(std::fs::read_link(&path).unwrap_or_else(|error| {
                            panic!("read_link {}: {error}", path.display())
                        })),
                        mode,
                        modified,
                    },
                );
            } else {
                out.insert(
                    rel,
                    TreeEntry {
                        kind: "other",
                        len: None,
                        content_hash: None,
                        symlink_target: None,
                        mode,
                        modified,
                    },
                );
            }
        }
    }

    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let mut perms = std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("metadata {}: {error}", path.display()))
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

fn write_version_shim(path: &Path, binary: &str) {
    let contents = format!(
        "#!/bin/sh\nprintf '%s\\n' '{binary} {}'\n",
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(path, contents)
        .unwrap_or_else(|error| panic!("write shim {}: {error}", path.display()));
    set_executable(path);
}

fn insert_project(conn: &mcp_agent_mail_db::DbConn, id: i64, slug: &str, human_key: &str) {
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::Text(slug.to_string()),
            SqlValue::Text(human_key.to_string()),
            SqlValue::BigInt(1_704_067_200_000_000), // 2024-01-01T00:00:00Z
        ],
    )
    .expect("insert project");
}

fn insert_message(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body: &str,
) {
    conn.execute_sync(
        "INSERT INTO messages (\
            id, project_id, sender_id, subject, body_md, importance, ack_required, \
            created_ts, thread_id\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::BigInt(sender_id),
            SqlValue::Text(subject.to_string()),
            SqlValue::Text(body.to_string()),
            SqlValue::Text("normal".to_string()),
            SqlValue::Bool(false),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::Null,
        ],
    )
    .expect("insert message");
}

fn insert_recipient(conn: &mcp_agent_mail_db::DbConn, message_id: i64, agent_id: i64) {
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[
            SqlValue::BigInt(message_id),
            SqlValue::BigInt(agent_id),
            SqlValue::Text("to".to_string()),
        ],
    )
    .expect("insert recipient");
}

fn seed_startup_recovery_orphan_recipient(env: &TestEnv) {
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    conn.execute_raw("PRAGMA foreign_keys = OFF")
        .expect("disable foreign keys for crash replay fixture");
    insert_project(
        &conn,
        1,
        "startup-recovery",
        &env.tmp.path().join("project").display().to_string(),
    );
    insert_agent(&conn, 1, 1, "Sender", "codex-cli", "gpt-5");
    insert_agent(&conn, 2, 1, "Recipient", "codex-cli", "gpt-5");
    insert_message(&conn, 1, 1, 1, "startup repair replay", "body");
    insert_recipient(&conn, 1, 2);
    insert_recipient(&conn, 999, 2);
    conn.close_sync()
        .expect("close startup recovery orphan fixture");
    mcp_agent_mail_db::pool::wal_checkpoint_truncate_path(&env.db_path)
        .expect("checkpoint startup recovery orphan fixture");
}

fn seed_startup_recovery_archive_only(env: &TestEnv) {
    let project_path = env.tmp.path().join("reconstructed-project");
    std::fs::create_dir_all(&project_path).expect("create reconstruct project path");
    let project_human_key = project_path.display().to_string();
    let project_slug = mcp_agent_mail_db::queries::generate_slug(&project_human_key);
    let project_dir = env.storage_root.join("projects").join(&project_slug);
    let sender_dir = project_dir.join("agents").join("Sender");
    let recipient_dir = project_dir.join("agents").join("Recipient");
    let message_dir = project_dir.join("messages").join("2026").join("05");
    std::fs::create_dir_all(&sender_dir).expect("create sender archive dir");
    std::fs::create_dir_all(&recipient_dir).expect("create recipient archive dir");
    std::fs::create_dir_all(&message_dir).expect("create message archive dir");
    std::fs::write(
        project_dir.join("project.json"),
        format!(
            r#"{{"slug":"{}","human_key":"{}"}}"#,
            project_slug, project_human_key
        ),
    )
    .expect("write reconstruct project metadata");
    std::fs::write(
        sender_dir.join("profile.json"),
        r#"{"name":"Sender","agent_name":"Sender","program":"codex-cli","model":"gpt-5","task_description":"startup reconstruct sender","inception_ts":"2026-05-12T00:00:00Z","last_active_ts":"2026-05-12T00:00:01Z"}"#,
    )
    .expect("write sender profile");
    std::fs::write(
        recipient_dir.join("profile.json"),
        r#"{"name":"Recipient","agent_name":"Recipient","program":"codex-cli","model":"gpt-5","task_description":"startup reconstruct recipient","inception_ts":"2026-05-12T00:00:02Z","last_active_ts":"2026-05-12T00:00:03Z"}"#,
    )
    .expect("write recipient profile");
    std::fs::write(
        message_dir.join("2026-05-12T00-00-04Z__startup-reconstruct__1.md"),
        r#"---json
{
  "id": 1,
  "from": "Sender",
  "from_agent": "Sender",
  "to": ["Recipient"],
  "cc": [],
  "bcc": [],
  "subject": "startup reconstruct replay",
  "importance": "normal",
  "ack_required": false,
  "thread_id": "startup-reconstruct",
  "created_ts": "2026-05-12T00:00:04Z",
  "attachments": []
}
---

reconstruct body
"#,
    )
    .expect("write reconstruct message archive");
}

fn insert_file_reservation(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    agent_id: i64,
    path: &str,
    exclusive: bool,
    expires_ts: i64,
) {
    conn.execute_sync(
        "INSERT INTO file_reservations (\
            id, project_id, agent_id, path_pattern, exclusive, reason, \
            created_ts, expires_ts, released_ts\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::BigInt(agent_id),
            SqlValue::Text(path.to_string()),
            SqlValue::Bool(exclusive),
            SqlValue::Text("test".to_string()),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::BigInt(expires_ts),
            SqlValue::Null,
        ],
    )
    .expect("insert file reservation");
}

fn insert_agent(
    conn: &mcp_agent_mail_db::DbConn,
    id: i64,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
) {
    conn.execute_sync(
        "INSERT INTO agents (\
            id, project_id, name, program, model, task_description, inception_ts, last_active_ts, \
            attachments_policy, contact_policy\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(id),
            SqlValue::BigInt(project_id),
            SqlValue::Text(name.to_string()),
            SqlValue::Text(program.to_string()),
            SqlValue::Text(model.to_string()),
            SqlValue::Text(String::new()),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::BigInt(1_704_067_200_000_000),
            SqlValue::Text("auto".to_string()),
            SqlValue::Text("auto".to_string()),
        ],
    )
    .expect("insert agent");
}

fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create git repo dir");
    let out = Command::new("git")
        .current_dir(path)
        .args(["init", "-b", "main"])
        .output()
        .expect("git init");
    assert!(
        out.status.success(),
        "git init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn seed_projects_for_adopt(env: &TestEnv, same_repo: bool) -> (String, String, String, String) {
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");

    let source_slug = "source-proj".to_string();
    let target_slug = "target-proj".to_string();

    let (source_path, target_path) = if same_repo {
        let repo_root = env.tmp.path().join("workspace");
        init_git_repo(&repo_root);
        let src = repo_root.join("source");
        let dst = repo_root.join("target");
        std::fs::create_dir_all(&src).expect("create source path");
        std::fs::create_dir_all(&dst).expect("create target path");
        (src, dst)
    } else {
        let src_repo = env.tmp.path().join("source_repo");
        let dst_repo = env.tmp.path().join("target_repo");
        init_git_repo(&src_repo);
        init_git_repo(&dst_repo);
        (src_repo, dst_repo)
    };

    let source_key = source_path.canonicalize().expect("canonical source path");
    let target_key = target_path.canonicalize().expect("canonical target path");
    let source_key_str = source_key.display().to_string();
    let target_key_str = target_key.display().to_string();
    insert_project(&conn, 1, &source_slug, &source_key_str);
    insert_project(&conn, 2, &target_slug, &target_key_str);

    (source_slug, target_slug, source_key_str, target_key_str)
}

fn assert_success(
    env: &TestEnv,
    case: &str,
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) {
    let out = run_am(&env.base_env(), cwd, args, stdin);
    if out.status.success() {
        return;
    }
    write_artifact(case, args, &out);
    panic!(
        "expected success for {case} args={args:?}, got status={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn startup_recovery_artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/startup_recovery_crash_replay")
}

fn installed_binary_parity_artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/installed_binary_parity")
}

fn support_bundle_redaction_artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/support_bundle_redaction_adversarial")
}

fn stale_handoff_artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/stale_handoff_dashboard")
}

fn write_text_artifact(run_root: &Path, name: &str, content: &str) {
    std::fs::write(run_root.join(name), content).expect("write text artifact");
}

fn write_json_artifact(run_root: &Path, name: &str, value: &Value) {
    let content = serde_json::to_string_pretty(value).expect("serialize JSON artifact");
    write_text_artifact(run_root, name, &format!("{content}\n"));
}

fn robot_search_index_state<'a>(value: &'a Value, context: &str) -> &'a str {
    value
        .get("search_index")
        .or_else(|| value.get("data").and_then(|data| data.get("search_index")))
        .and_then(|search_index| search_index.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing robot search_index.state in {context}: {value}"))
}

fn robot_total_results(value: &Value, context: &str) -> u64 {
    value
        .get("total_results")
        .or_else(|| value.get("data").and_then(|data| data.get("total_results")))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing robot total_results in {context}: {value}"))
}

fn doctor_non_ok_checks(value: &Value) -> Value {
    Value::Array(
        value
            .get("checks")
            .and_then(Value::as_array)
            .map(|checks| {
                checks
                    .iter()
                    .filter(|check| {
                        check
                            .get("status")
                            .and_then(Value::as_str)
                            .is_some_and(|status| status != "ok")
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn doctor_non_environment_fail_checks(value: &Value) -> Value {
    Value::Array(
        value
            .get("checks")
            .and_then(Value::as_array)
            .map(|checks| {
                checks
                    .iter()
                    .filter(|check| {
                        check.get("status").and_then(Value::as_str) == Some("fail")
                            && check.get("category").and_then(Value::as_str) != Some("environment")
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn doctor_check_status<'a>(value: &'a Value, check_name: &str) -> Option<&'a str> {
    value
        .get("checks")
        .and_then(Value::as_array)?
        .iter()
        .find(|check| check.get("check").and_then(Value::as_str) == Some(check_name))
        .and_then(|check| check.get("status"))
        .and_then(Value::as_str)
}

fn assert_doctor_mailbox_recovered(value: &Value, context: &str) {
    for check_name in [
        "database",
        "db_file_sanity",
        "pool_init",
        "foreign_key_integrity",
    ] {
        assert_eq!(
            doctor_check_status(value, check_name),
            Some("ok"),
            "{context}: expected {check_name} to be ok; non-ok checks:\n{}",
            serde_json::to_string_pretty(&doctor_non_ok_checks(value)).unwrap()
        );
    }

    let non_environment_failures = doctor_non_environment_fail_checks(value);
    assert!(
        non_environment_failures
            .as_array()
            .is_some_and(Vec::is_empty),
        "{context}: doctor reported non-environment failures after recovery:\n{}",
        serde_json::to_string_pretty(&non_environment_failures).unwrap()
    );
}

fn response_by_id(responses: &[Value], id: i64) -> Option<&Value> {
    responses
        .iter()
        .find(|response| response.get("id").and_then(Value::as_i64) == Some(id))
}

fn response_is_error(response: &Value) -> bool {
    response.get("error").is_some()
        || response
            .get("result")
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

#[derive(Debug)]
struct InstalledParitySurface {
    id: &'static str,
    args: &'static [&'static str],
    required_paths: &'static [&'static str],
}

const INSTALLED_PARITY_SURFACES: &[InstalledParitySurface] = &[
    InstalledParitySurface {
        id: "doctor_check",
        args: &["doctor", "check", "--json"],
        required_paths: &[
            "forensic_timeline.schema",
            "forensic_timeline.current.binary_version",
            "forensic_timeline.current.schema_version",
            "forensic_timeline.current.storage_root",
            "forensic_timeline.current.database_path",
            "forensic_timeline.current.search_index_generation.state",
            "forensic_timeline.recent_recovery_runs",
            "forensic_timeline.artifacts",
            "forensic_timeline.next_actions",
        ],
    },
    InstalledParitySurface {
        id: "robot_status",
        args: &[
            "robot",
            "status",
            "--agent",
            "Recipient",
            "--format",
            "json",
        ],
        required_paths: &[
            "forensic_timeline.schema",
            "forensic_timeline.current.binary_version",
            "forensic_timeline.current.search_index_generation.state",
        ],
    },
    InstalledParitySurface {
        id: "robot_search",
        args: &["robot", "search", "parity-probe", "--format", "json"],
        required_paths: &["search_index.state"],
    },
];

fn parity_json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    (!current.is_null()).then_some(current)
}

fn redacted_parity_text(text: &str, storage_root: &Path, database_url: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.contains("bearer")
        || lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
    {
        return "<redacted>".to_string();
    }

    let mut redacted = text.replace(&repo_root().display().to_string(), "<repo_root>");
    redacted = redacted.replace(&storage_root.display().to_string(), "<storage_root>");
    redacted.replace(database_url, "<database_url>")
}

fn redacted_parity_value(value: Option<&Value>, storage_root: &Path, database_url: &str) -> Value {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(text)) => {
            Value::String(redacted_parity_text(text, storage_root, database_url))
        }
        Some(Value::Array(items)) => Value::Array(
            items
                .iter()
                .map(|item| redacted_parity_value(Some(item), storage_root, database_url))
                .collect(),
        ),
        Some(Value::Object(map)) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    (
                        key.clone(),
                        redacted_parity_value(Some(item), storage_root, database_url),
                    )
                })
                .collect(),
        ),
        Some(other) => other.clone(),
    }
}

fn parity_version(outputs: &Value) -> Value {
    parity_json_path(
        outputs,
        "doctor_check.forensic_timeline.current.binary_version",
    )
    .cloned()
    .unwrap_or(Value::Null)
}

fn source_git_commit() -> Value {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_root())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if commit.is_empty() {
                Value::Null
            } else {
                Value::String(commit)
            }
        }
        _ => Value::Null,
    }
}

fn installed_binary_parity_report(
    source_binary: &Path,
    installed_binary: &Path,
    storage_root: &Path,
    database_url: &str,
    source_outputs: &Value,
    installed_outputs: &Value,
) -> Value {
    let mut checks = Vec::new();
    for surface in INSTALLED_PARITY_SURFACES {
        let source_surface = source_outputs.get(surface.id).unwrap_or(&Value::Null);
        let installed_surface = installed_outputs.get(surface.id).unwrap_or(&Value::Null);
        for path in surface.required_paths {
            let source_value = parity_json_path(source_surface, path);
            let installed_value = parity_json_path(installed_surface, path);
            let status = match (source_value, installed_value) {
                (Some(source), Some(installed)) if source == installed => "pass",
                (Some(_), Some(_)) => "value_mismatch",
                (Some(_), None) => "installed_missing",
                (None, Some(_)) => "source_missing",
                (None, None) => "missing_both",
            };
            checks.push(json!({
                "surface": surface.id,
                "json_path": path,
                "status": status,
                "source_value": redacted_parity_value(source_value, storage_root, database_url),
                "installed_value": redacted_parity_value(installed_value, storage_root, database_url),
            }));
        }
    }

    let passed = checks
        .iter()
        .all(|check| check.get("status").and_then(Value::as_str) == Some("pass"));
    let commands: Vec<Value> = INSTALLED_PARITY_SURFACES
        .iter()
        .map(|surface| {
            json!({
                "surface": surface.id,
                "source_command": std::iter::once(redacted_parity_text(
                    &source_binary.display().to_string(),
                    storage_root,
                    database_url,
                ))
                .chain(surface.args.iter().map(|arg| (*arg).to_string()))
                .collect::<Vec<_>>(),
                "installed_command": std::iter::once(redacted_parity_text(
                    &installed_binary.display().to_string(),
                    storage_root,
                    database_url,
                ))
                .chain(surface.args.iter().map(|arg| (*arg).to_string()))
                .collect::<Vec<_>>(),
            })
        })
        .collect();

    json!({
        "schema": "installed-binary-parity.v1",
        "bead": "br-idea-wizard-swarm-reliability-2ac6x.4",
        "passed": passed,
        "metadata": {
            "source_binary": redacted_parity_text(
                &source_binary.display().to_string(),
                storage_root,
                database_url,
            ),
            "source_version": parity_version(source_outputs),
            "source_git_commit": source_git_commit(),
            "installed_binary": redacted_parity_text(
                &installed_binary.display().to_string(),
                storage_root,
                database_url,
            ),
            "installed_version": parity_version(installed_outputs),
            "installed_git_commit": Value::Null,
            "storage_root": "<storage_root>",
            "database_url": "<database_url>",
            "database_path": "<database_path>",
        },
        "commands": commands,
        "checks": checks,
    })
}

fn collect_installed_parity_outputs(binary: &Path, env: &TestEnv) -> Value {
    let mut outputs = serde_json::Map::new();
    for surface in INSTALLED_PARITY_SURFACES {
        let out = run_am_binary(
            binary,
            &env.base_env(),
            Some(env.tmp.path()),
            surface.args,
            None,
        );
        assert!(
            out.status.success(),
            "{} parity command failed for {}\nstdout:\n{}\nstderr:\n{}",
            surface.id,
            binary.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "{} parity command emitted invalid JSON for {}: {error}\nstdout:\n{}",
                surface.id,
                binary.display(),
                String::from_utf8_lossy(&out.stdout)
            )
        });
        outputs.insert(surface.id.to_string(), parsed);
    }
    Value::Object(outputs)
}

fn run_startup_server_once(env: &TestEnv) -> TimedProcessOutput {
    run_startup_server_once_at(env, env.tmp.path())
}

fn run_startup_server_once_at(env: &TestEnv, cwd: &Path) -> TimedProcessOutput {
    let env_vars = env.isolated_env();
    run_startup_server_once_at_with_env(&env_vars, cwd)
}

fn run_startup_server_once_at_with_env(
    env_vars: &[(String, String)],
    cwd: &Path,
) -> TimedProcessOutput {
    let startup_args = ["serve-stdio"];
    run_am_with_timeout(env_vars, Some(cwd), &startup_args, Duration::from_secs(60))
}

#[test]
fn capabilities_json_exposes_agent_contract() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["capabilities", "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact("capabilities_json", &["capabilities", "--json"], &out);
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["schema_version"].as_str(), Some("am.capabilities.v1"));
    assert_eq!(
        value["primary_agent_surfaces"]["capabilities"].as_str(),
        Some("am capabilities --json")
    );
    assert_eq!(
        value["primary_agent_surfaces"]["agent_cockpit"].as_str(),
        Some("am agent start --json")
    );
    assert_eq!(
        value["primary_agent_surfaces"]["status"].as_str(),
        Some("am status --project /abs/path --agent AGENT_NAME --json")
    );
    let exit_codes = value["exit_codes"]
        .as_array()
        .expect("exit_codes should be an array");
    assert!(
        exit_codes.iter().any(|entry| {
            entry["code"].as_i64() == Some(i64::from(mcp_agent_mail_cli::LEGACY_AM_SERVE_EXIT_CODE))
                && entry["meaning"]
                    .as_str()
                    .is_some_and(|meaning| meaning.contains("legacy CLI subcommand migration"))
        }),
        "capabilities should expose the legacy am serve migration exit code"
    );
    let primary_surfaces = value["primary_agent_surfaces"]
        .as_object()
        .expect("primary_agent_surfaces should be an object");
    assert!(
        primary_surfaces.values().all(|command| {
            let command = command.as_str().unwrap_or_default();
            !command.contains('<') && !command.contains('>')
        }),
        "machine-readable command recipes should avoid shell metacharacter placeholders"
    );
    let commands = value["commands"]
        .as_array()
        .expect("commands should be an array");
    let corrections = value["mcp_tool_cli_corrections"]
        .as_array()
        .expect("mcp_tool_cli_corrections should be an array");
    for expected in mcp_agent_mail_cli::mcp_tool_cli_corrections() {
        let actual = corrections
            .iter()
            .find(|entry| entry["cli"].as_str() == Some(expected.cli))
            .unwrap_or_else(|| panic!("missing correction for {}", expected.cli));
        let attempted_names = actual["attempted_names"]
            .as_array()
            .expect("attempted_names should be an array");
        for attempted in expected.attempted_names {
            assert!(
                attempted_names
                    .iter()
                    .any(|value| value.as_str() == Some(*attempted)),
                "capabilities correction for {} should include attempted name {}",
                expected.cli,
                attempted
            );
        }
        assert_eq!(actual["mcp_tool"].as_str(), expected.mcp_tool);
    }
    assert!(
        commands.iter().any(|command| {
            command["name"].as_str() == Some("robot status")
                && command["recommended_for_agents"].as_bool() == Some(true)
                && command["supports_json_flag"].as_bool() == Some(true)
        }),
        "robot status should be present, agent-recommended, and advertise --json"
    );
    assert!(
        commands.iter().any(|command| {
            command["name"].as_str() == Some("robot")
                && command["supports_json_flag"].as_bool() == Some(true)
                && command["output_formats"].as_array().is_some_and(|formats| {
                    formats == &vec![Value::from("toon"), Value::from("json"), Value::from("md")]
                })
        }),
        "robot parent command should advertise robot formats, not generic table output"
    );
    assert!(
        commands.iter().any(|command| {
            command["name"].as_str() == Some("status")
                && command["recommended_for_agents"].as_bool() == Some(true)
        }),
        "top-level status alias should be present and marked agent-recommended"
    );
    assert!(
        commands
            .iter()
            .any(|command| command["name"].as_str() == Some("robot-docs guide")),
        "robot-docs guide should be present in the command catalog"
    );
}

#[test]
fn root_help_exposes_mcp_tool_correction_catalog() {
    let env = TestEnv::new();
    let out = run_am(&env.base_env(), Some(env.tmp.path()), &["--help"], None);
    if !out.status.success() {
        write_artifact("root_help", &["--help"], &out);
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8(out.stdout).expect("stdout should be utf-8");
    assert!(stdout.contains("MCP tool-name corrections:"));
    let compact_stdout = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    for correction in mcp_agent_mail_cli::mcp_tool_cli_corrections() {
        let compact_cli = correction
            .cli
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            compact_stdout.contains(&compact_cli),
            "root help should expose correction `{}`.\nstdout:\n{}",
            correction.cli,
            stdout
        );
        for attempted in correction.attempted_names {
            assert!(
                stdout.contains(attempted),
                "root help should mention MCP-style attempted name `{attempted}`"
            );
        }
    }
}

#[test]
fn robot_status_accepts_json_shorthand() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["robot", "status", "--json", "--help"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "robot_status_json_help",
            &["robot", "status", "--json", "--help"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8(out.stdout).expect("stdout should be utf-8");
    assert!(
        stdout.contains("--json"),
        "robot status help should document --json shorthand"
    );
}

#[test]
fn tooling_directory_json_exposes_mcp_tool_corrections() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["tooling", "directory", "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "tooling_directory_json",
            &["tooling", "directory", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let corrections = value["mcp_tool_cli_corrections"]
        .as_array()
        .expect("mcp_tool_cli_corrections should be an array");
    for expected in mcp_agent_mail_cli::mcp_tool_cli_corrections() {
        assert!(
            corrections
                .iter()
                .any(|entry| entry["cli"].as_str() == Some(expected.cli)),
            "tooling directory should expose correction `{}`",
            expected.cli
        );
    }
}

#[test]
fn robot_status_markdown_rejection_is_usage_error() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["robot", "status", "--format", "md"],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid robot format should exit as usage error\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).expect("stderr should be utf-8");
    assert!(
        stderr.contains("usage error")
            && stderr.contains("--format md is only supported for `am robot thread`"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn agent_start_json_surfaces_first_turn_cockpit() {
    let env = TestEnv::new();
    let project = env.tmp.path().display().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--project",
            &project,
            "--agent",
            "BlueLake",
            "--program",
            "codex-cli",
            "--model",
            "gpt-5.5",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact("agent_start_json", &["agent", "start", "--json"], &out);
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["schema_version"].as_str(), Some("am.agent_start.v1"));
    assert_eq!(value["project"]["key"].as_str(), Some(project.as_str()));
    assert_eq!(value["agent"]["name"].as_str(), Some("BlueLake"));
    assert_eq!(value["readiness"]["agent_ready"].as_bool(), Some(true));
    let expected_status = format!("am status --project {project} --agent BlueLake --json");
    assert_eq!(
        value["commands"]["status"].as_str(),
        Some(expected_status.as_str())
    );
    let next_actions = value["next_actions"]
        .as_array()
        .expect("next_actions should be an array");
    assert!(
        next_actions
            .iter()
            .any(|action| action["id"].as_str() == Some("check_status")),
        "agent start should recommend checking status"
    );
}

#[test]
fn agent_start_json_reports_resolved_runtime_config() {
    let env = TestEnv::new();
    let project = env.tmp.path().display().to_string();
    let mut env_vars = env.base_env();
    env_vars.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "HTTP_HOST" | "HTTP_PORT" | "HTTP_PATH" | "HTTP_BEARER_TOKEN"
        )
    });
    env_vars.extend([
        ("HTTP_HOST".to_string(), "0.0.0.0".to_string()),
        ("HTTP_PORT".to_string(), "9123".to_string()),
        ("HTTP_PATH".to_string(), "api".to_string()),
        ("HTTP_BEARER_TOKEN".to_string(), "test-token".to_string()),
    ]);

    let out = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--project",
            &project,
            "--agent",
            "BlueLake",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_runtime_config",
            &["agent", "start", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["runtime"]["http_host"].as_str(), Some("0.0.0.0"));
    assert_eq!(value["runtime"]["http_port"].as_str(), Some("9123"));
    assert_eq!(value["runtime"]["http_path"].as_str(), Some("/api/"));
    assert_eq!(
        value["runtime"]["http_url"].as_str(),
        Some("http://127.0.0.1:9123/api/")
    );
    assert_eq!(
        value["runtime"]["bearer_token_configured"].as_bool(),
        Some(true)
    );
    assert_eq!(value["runtime"]["auth_enabled"].as_bool(), Some(true));
    assert_eq!(value["runtime"]["http_jwt_enabled"].as_bool(), Some(false));
    let expected_database_url = env.database_url();
    assert_eq!(
        value["runtime"]["database_url"].as_str(),
        Some(expected_database_url.as_str())
    );
}

#[test]
fn agent_start_json_reports_jwt_auth_mode() {
    let env = TestEnv::new();
    let project = env.tmp.path().display().to_string();
    let mut env_vars = env.hermetic_env();
    env_vars.extend([
        ("DATABASE_URL".to_string(), env.database_url()),
        (
            "STORAGE_ROOT".to_string(),
            env.storage_root.display().to_string(),
        ),
        (
            "AM_ALLOW_EPHEMERAL_PROJECT_ROOTS".to_string(),
            "1".to_string(),
        ),
        ("HTTP_JWT_ENABLED".to_string(), "true".to_string()),
        ("HTTP_JWT_SECRET".to_string(), "test-secret".to_string()),
    ]);

    let out = run_am_hermetic(
        &env_vars,
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--project",
            &project,
            "--agent",
            "BlueLake",
            "--json",
        ],
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_jwt_auth_mode",
            &["agent", "start", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["runtime"]["auth_enabled"].as_bool(), Some(true));
    assert_eq!(value["runtime"]["http_jwt_enabled"].as_bool(), Some(true));
    assert_eq!(
        value["runtime"]["bearer_token_configured"].as_bool(),
        Some(false)
    );
}

#[test]
fn agent_start_json_missing_agent_uses_shell_safe_placeholder() {
    let env = TestEnv::new();
    let project = env.tmp.path().display().to_string();
    let mut env_vars = env.base_env();
    env_vars.retain(|(key, _)| !matches!(key.as_str(), "AGENT_NAME" | "AGENT_MAIL_AGENT"));

    let out = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &["agent", "start", "--project", &project, "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_missing_agent_placeholder",
            &["agent", "start", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["agent"]["source"].as_str(), Some("missing"));
    assert_eq!(value["readiness"]["agent_ready"].as_bool(), Some(false));
    let expected_status = format!("am status --project {project} --agent AGENT_NAME --json");
    assert_eq!(
        value["commands"]["status"].as_str(),
        Some(expected_status.as_str())
    );
    let serialized = serde_json::to_string(&value).expect("serialize agent start JSON");
    assert!(
        !serialized.contains("<AgentName>") && !serialized.contains("<thread_id>"),
        "agent start JSON should avoid shell metacharacter placeholders"
    );
}

#[test]
fn agent_start_fix_idempotently_registers_identity() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    std::fs::create_dir_all(&env.storage_root).expect("create storage root");
    let project_root = env.tmp.path().join("agent-fix-project");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let project_key = project_root.display().to_string();

    for _ in 0..2 {
        let out = run_am(
            &env.base_env(),
            Some(env.tmp.path()),
            &[
                "agent",
                "start",
                "--fix",
                "--project",
                &project_key,
                "--agent",
                "BlueLake",
                "--program",
                "codex-cli",
                "--model",
                "gpt-5",
                "--json",
            ],
            None,
        );
        if !out.status.success() {
            write_artifact(
                "agent_start_fix_registers_identity",
                &["agent", "start", "--fix", "--json"],
                &out,
            );
            panic!(
                "expected success\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
        assert_eq!(value["fix"]["requested"].as_bool(), Some(true));
        assert_eq!(value["fix"]["status"].as_str(), Some("applied"));
        let action_ids: Vec<_> = value["fix"]["actions"]
            .as_array()
            .expect("fix actions")
            .iter()
            .filter_map(|action| action["id"].as_str())
            .collect();
        assert_eq!(
            action_ids,
            vec!["ensure_project", "register_agent", "inbox_probe"]
        );
    }

    let list = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["agents", "list", "-p", &project_key, "--json"],
        None,
    );
    if !list.status.success() {
        write_artifact(
            "agent_start_fix_agents_list",
            &["agents", "list", "-p", "--json"],
            &list,
        );
        panic!(
            "expected agents list success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&list.stdout),
            String::from_utf8_lossy(&list.stderr)
        );
    }
    let agents: Value = serde_json::from_slice(&list.stdout).expect("valid agents JSON");
    let rows = agents.as_array().expect("agents list array");
    assert_eq!(
        rows.len(),
        1,
        "re-running --fix should not duplicate agents"
    );
    assert_eq!(rows[0]["name"].as_str(), Some("BlueLake"));
}

#[test]
fn agent_start_fix_blocks_without_safe_prerequisites() {
    let env = TestEnv::new();
    let mut env_vars = env.base_env();
    env_vars.retain(|(key, _)| !matches!(key.as_str(), "AGENT_NAME" | "AGENT_MAIL_AGENT"));

    let out = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--fix",
            "--project",
            "relative-project",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_fix_blocked_prerequisites",
            &["agent", "start", "--fix", "--json"],
            &out,
        );
        panic!(
            "expected blocked report with success exit\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["fix"]["requested"].as_bool(), Some(true));
    assert_eq!(value["fix"]["status"].as_str(), Some("blocked"));
    assert!(
        value["fix"]["blocked_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("existing absolute path")),
        "blocked fix should explain the project preflight failure"
    );
}

#[test]
fn agent_start_json_reports_stale_mcp_endpoint() {
    let env = TestEnv::new();
    let project = env.tmp.path().display().to_string();
    let port = unused_loopback_port().to_string();
    let mut env_vars = env.base_env();
    env_vars.retain(|(key, _)| key != "HTTP_PORT");
    env_vars.push(("HTTP_PORT".to_string(), port.clone()));

    let out = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--project",
            &project,
            "--agent",
            "BlueLake",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_stale_mcp_endpoint",
            &["agent", "start", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let mcp_check = json_check(&value, "mcp_endpoint");
    let expected_url = format!("http://127.0.0.1:{port}/mcp/");
    assert_eq!(mcp_check["status"].as_str(), Some("fail"));
    assert!(
        mcp_check["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("no listener is present")),
        "stale endpoint check should explain the missing listener"
    );
    assert_eq!(mcp_check["command"].as_str(), Some("am doctor health"));
    assert_eq!(
        value["runtime"]["http_url"].as_str(),
        Some(expected_url.as_str())
    );
}

#[test]
fn bare_am_no_args_noninteractive_emits_status_surface() {
    let env = TestEnv::new();
    let out = run_am(&env.isolated_env(), Some(env.tmp.path()), &[], None);
    if !out.status.success() {
        write_artifact("bare_am_no_args_status_surface", &[], &out);
        panic!(
            "expected bare am success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Missing command") && !stderr.contains("Usage:"),
        "bare am must not emit generic usage/missing-command text; stderr:\n{stderr}"
    );

    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "bare am stdout must be JSON: {error}\nstdout:\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(value["_meta"]["command"].as_str(), Some("am"));
    assert_eq!(value["schema_version"].as_str(), Some("am.bare_status.v1"));
    assert_eq!(value["mode"].as_str(), Some("cli"));
    assert_eq!(value["binary"]["name"].as_str(), Some("am"));
    assert_eq!(
        value["binary"]["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(
        value["binary"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/am") || path.ends_with("\\am.exe")),
        "binary path should identify the resolved am executable: {value}"
    );
    assert_eq!(
        value["runtime"]["storage_root"].as_str(),
        Some(env.storage_root.to_string_lossy().as_ref())
    );
    assert_eq!(
        value["runtime"]["database_path"].as_str(),
        Some(env.db_path.to_string_lossy().as_ref())
    );
    assert_eq!(
        value["service"]["http_url"].as_str(),
        Some("http://127.0.0.1:1/mcp/")
    );
    assert_eq!(
        value["doctor_health"]["command"].as_str(),
        Some("am doctor health")
    );
    assert!(
        value["top_commands"]
            .as_array()
            .is_some_and(|commands| commands.iter().any(|command| command == "am status --json")),
        "top commands should include the obvious status command: {value}"
    );
    assert!(
        value["_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|action| action == "am doctor health")),
        "actions should include doctor health: {value}"
    );
}

#[test]
fn agent_start_json_reports_active_reservation_conflicts() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    let project_root = env.tmp.path().join("agent-conflict-project");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let project_key = project_root.display().to_string();
    insert_project(&conn, 1, "agent-conflict-project", &project_key);
    insert_agent(&conn, 1, 1, "BlueLake", "codex-cli", "gpt-5");
    insert_agent(&conn, 2, 1, "GreenCastle", "codex-cli", "gpt-5");
    let far_future = mcp_agent_mail_db::timestamps::now_micros() + 3_600_000_000;
    insert_file_reservation(&conn, 1, 1, 2, "src/**", true, far_future);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "agent",
            "start",
            "--project",
            &project_key,
            "--agent",
            "BlueLake",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agent_start_reservation_conflicts",
            &["agent", "start", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let reservation_check = json_check(&value, "reservation_conflicts");
    assert_eq!(reservation_check["status"].as_str(), Some("warn"));
    assert!(
        reservation_check["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("GreenCastle on src/**")),
        "reservation conflict detail should name the holder and pattern"
    );
    assert!(
        reservation_check["command"]
            .as_str()
            .is_some_and(|command| command.contains("am reservations --project")),
        "reservation conflict check should include the inspection command"
    );
}

#[test]
fn robot_docs_guide_prints_agent_handbook() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["robot-docs", "guide"],
        None,
    );
    if !out.status.success() {
        write_artifact("robot_docs_guide", &["robot-docs", "guide"], &out);
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8(out.stdout).expect("stdout should be utf-8");
    assert!(stdout.contains("am agent start --json"));
    assert!(stdout.contains("am capabilities --json"));
    assert!(stdout.contains("am status --project /abs/path --agent AGENT_NAME"));
    assert!(stdout.contains("am tooling schemas --json"));
    assert!(stdout.contains("Broadcast send_message is intentionally unsupported."));
    assert!(
        !stdout.contains("<AgentName>") && !stdout.contains("<thread_id>"),
        "rendered guide should not emit shell metacharacter placeholders"
    );
}

#[test]
fn robot_docs_guide_json_exposes_precise_schema() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["robot-docs", "guide", "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "robot_docs_guide_json",
            &["robot-docs", "guide", "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(
        value["schema_version"].as_str(),
        Some("am.robot_docs.guide.v1")
    );
    assert!(
        value["quick_start"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command.as_str() == Some("am agent start --json"))),
        "guide should include the first-turn cockpit in quick_start"
    );
    let serialized = serde_json::to_string(&value).expect("serialize guide JSON");
    assert!(
        !serialized.contains("<AgentName>")
            && !serialized.contains("<thread_id>")
            && !serialized.contains("<query>")
            && !serialized.contains("<id>")
            && !serialized.contains("<iso8601>"),
        "guide JSON should avoid shell metacharacter placeholders"
    );
}

#[test]
fn migrate_then_list_projects_json_smoke() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    assert_success(&env, "migrate", Some(env.tmp.path()), &["migrate"], None);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["list-projects", "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact("list_projects_json", &["list-projects", "--json"], &out);
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(
        value.is_array(),
        "expected JSON array, got: {}",
        serde_json::to_string_pretty(&value).unwrap()
    );
}

#[test]
fn agents_list_json_by_human_key_smoke() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");

    let project_root = env.tmp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let project_key = project_root.display().to_string();

    insert_project(&conn, 1, "tmp-project", &project_key);
    insert_agent(&conn, 1, 1, "BlueLake", "codex-cli", "gpt-5");
    drop(conn);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["agents", "list", "-p", &project_key, "--json"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "agents_list_json_by_human_key",
            &["agents", "list", "-p", &project_key, "--json"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let rows = value
        .as_array()
        .expect("agents list should be a JSON array");
    assert_eq!(rows.len(), 1, "expected exactly one agent row");
    assert_eq!(rows[0]["name"].as_str(), Some("BlueLake"));
}

#[test]
fn macros_start_session_json_smoke() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    std::fs::create_dir_all(&env.storage_root).expect("create storage root");

    let project_root = env.tmp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let project_key = project_root.display().to_string();

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "macros",
            "start-session",
            "-p",
            &project_key,
            "--program",
            "codex-cli",
            "--model",
            "gpt-5",
            "--task",
            "integration smoke",
            "--json",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "macros_start_session_json_smoke",
            &[
                "macros",
                "start-session",
                "-p",
                &project_key,
                "--program",
                "codex-cli",
                "--model",
                "gpt-5",
                "--task",
                "integration smoke",
                "--json",
            ],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(
        value["project"]["human_key"].as_str(),
        Some(project_key.as_str())
    );
    assert!(
        value["agent"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "expected non-empty agent name"
    );
    assert!(
        value["inbox"].is_array(),
        "expected inbox array in start-session response"
    );
}

#[test]
fn guard_install_status_uninstall_smoke() {
    let env = TestEnv::new();
    let repo = env.tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    // Guard expects a git repo (hooks dir lives under .git/hooks by default).
    let git = Command::new("git")
        .current_dir(&repo)
        .args(["init", "-b", "main"])
        .output()
        .expect("git init");
    assert!(
        git.status.success(),
        "git init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&git.stdout),
        String::from_utf8_lossy(&git.stderr)
    );

    let repo_str = repo.to_string_lossy().to_string();

    // Install.
    let install_out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["guard", "install", "my-project", &repo_str],
        None,
    );
    assert!(
        install_out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&install_out.stdout).contains("Guard installed successfully."),
        "missing success marker"
    );
    let precommit = repo.join(".git").join("hooks").join("pre-commit");
    assert!(
        precommit.exists(),
        "expected hook at {}",
        precommit.display()
    );
    let precommit_body = std::fs::read_to_string(&precommit).expect("read pre-commit hook");
    assert!(
        precommit_body.contains("mcp-agent-mail chain-runner (pre-commit)"),
        "unexpected pre-commit body:\n{precommit_body}"
    );

    // Status.
    let status_out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["guard", "status", &repo_str],
        None,
    );
    assert!(
        status_out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status_out.stdout),
        String::from_utf8_lossy(&status_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status_out.stdout).contains("Guard Status:"),
        "expected status header"
    );

    // Uninstall.
    let uninstall_out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["guard", "uninstall", &repo_str],
        None,
    );
    assert!(
        uninstall_out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&uninstall_out.stdout),
        String::from_utf8_lossy(&uninstall_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&uninstall_out.stdout).contains("Guard uninstalled successfully."),
        "missing uninstall success marker"
    );
}

#[test]
fn guard_check_conflict_exits_1_when_not_advisory() {
    let env = TestEnv::new();
    let repo = env.tmp.path().join("archive_root");
    std::fs::create_dir_all(repo.join("file_reservations")).expect("create file_reservations dir");

    // Active exclusive reservation held by someone else.
    let reservation = serde_json::json!({
        "path_pattern": "foo.txt",
        "agent_name": "OtherAgent",
        "exclusive": true,
        "expires_ts": "2999-01-01T00:00:00Z",
        "released_ts": serde_json::Value::Null,
    });
    std::fs::write(
        repo.join("file_reservations").join("res.json"),
        serde_json::to_string_pretty(&reservation).unwrap(),
    )
    .expect("write reservation");

    let repo_str = repo.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["guard", "check", "--repo", &repo_str],
        Some(b"foo.txt\n"),
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CONFLICT: pattern"),
        "expected conflict marker in stderr, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn guard_check_advisory_does_not_exit_1() {
    let env = TestEnv::new();
    let repo = env.tmp.path().join("archive_root");
    std::fs::create_dir_all(repo.join("file_reservations")).expect("create file_reservations dir");

    let reservation = serde_json::json!({
        "path_pattern": "foo.txt",
        "agent_name": "OtherAgent",
        "exclusive": true,
        "expires_ts": "2999-01-01T00:00:00Z",
        "released_ts": serde_json::Value::Null,
    });
    std::fs::write(
        repo.join("file_reservations").join("res.json"),
        serde_json::to_string_pretty(&reservation).unwrap(),
    )
    .expect("write reservation");

    let repo_str = repo.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["guard", "check", "--advisory", "--repo", &repo_str],
        Some(b"foo.txt\n"),
    );
    assert!(
        out.status.success(),
        "expected success with --advisory\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CONFLICT: pattern"),
        "expected conflict marker in stderr, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn projects_mark_identity_no_commit_writes_marker_file() {
    let env = TestEnv::new();
    let project = env.tmp.path().join("proj_mark_identity");
    std::fs::create_dir_all(&project).expect("create project dir");

    let project_str = project.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "mark-identity", &project_str, "--no-commit"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_mark_identity_no_commit",
            &["projects", "mark-identity", &project_str, "--no-commit"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let marker = project.join(".agent-mail-project-id");
    assert!(
        marker.exists(),
        "expected marker file at {}",
        marker.display()
    );
    let marker_body = std::fs::read_to_string(&marker).expect("read marker file");
    assert!(
        !marker_body.trim().is_empty(),
        "expected non-empty project UID marker"
    );
}

#[test]
fn projects_mark_identity_default_commit_creates_git_commit() {
    let env = TestEnv::new();
    let project = env.tmp.path().join("proj_mark_identity_commit");
    init_git_repo(&project);

    let project_str = project.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "mark-identity", &project_str],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_mark_identity_default_commit",
            &["projects", "mark-identity", &project_str],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let marker = project.join(".agent-mail-project-id");
    assert!(
        marker.exists(),
        "expected marker file at {}",
        marker.display()
    );

    let log = Command::new("git")
        .current_dir(&project)
        .args(["log", "-1", "--pretty=%s"])
        .output()
        .expect("git log -1");
    assert!(
        log.status.success(),
        "expected git log success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&log.stdout),
        String::from_utf8_lossy(&log.stderr)
    );
    let subject = String::from_utf8_lossy(&log.stdout);
    assert_eq!(
        subject.trim(),
        "chore: add .agent-mail-project-id",
        "unexpected commit subject"
    );
}

#[test]
fn projects_discovery_init_writes_yaml_with_product_uid() {
    let env = TestEnv::new();
    let project = env.tmp.path().join("proj_discovery");
    std::fs::create_dir_all(&project).expect("create project dir");

    let project_str = project.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "projects",
            "discovery-init",
            &project_str,
            "--product",
            "product-xyz",
        ],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_discovery_init_with_product",
            &[
                "projects",
                "discovery-init",
                &project_str,
                "--product",
                "product-xyz",
            ],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let yaml = project.join(".agent-mail.yaml");
    assert!(
        yaml.exists(),
        "expected discovery file at {}",
        yaml.display()
    );
    let body = std::fs::read_to_string(&yaml).expect("read discovery file");
    assert!(
        body.contains("project_uid:"),
        "expected project_uid in discovery file:\n{body}"
    );
    assert!(
        body.contains("product_uid: product-xyz"),
        "expected product_uid in discovery file:\n{body}"
    );
}

#[test]
fn projects_discovery_init_without_product_omits_product_uid() {
    let env = TestEnv::new();
    let project = env.tmp.path().join("proj_discovery_no_product");
    std::fs::create_dir_all(&project).expect("create project dir");

    let project_str = project.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "discovery-init", &project_str],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_discovery_init_without_product",
            &["projects", "discovery-init", &project_str],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let yaml = project.join(".agent-mail.yaml");
    assert!(
        yaml.exists(),
        "expected discovery file at {}",
        yaml.display()
    );
    let body = std::fs::read_to_string(&yaml).expect("read discovery file");
    assert!(
        body.contains("project_uid:"),
        "expected project_uid in discovery file:\n{body}"
    );
    assert!(
        !body.contains("product_uid:"),
        "did not expect product_uid in discovery file:\n{body}"
    );
}

#[test]
fn projects_adopt_dry_run_prints_plan_and_leaves_artifacts_unchanged() {
    let env = TestEnv::new();
    let (source_slug, target_slug, source_key, target_key) = seed_projects_for_adopt(&env, true);
    let source_archive_file = env
        .storage_root
        .join("projects")
        .join(&source_slug)
        .join("messages")
        .join("source-message.md");
    std::fs::create_dir_all(source_archive_file.parent().expect("parent")).expect("create dir");
    std::fs::write(&source_archive_file, "hello").expect("write source archive file");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_key, &target_key],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_adopt_dry_run",
            &["projects", "adopt", &source_key, &target_key],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Projects adopt plan (dry-run)"),
        "missing dry-run marker in stdout:\n{}",
        stdout
    );
    assert!(
        source_archive_file.exists(),
        "dry-run should not move source artifacts"
    );
    let target_archive_file = env
        .storage_root
        .join("projects")
        .join(&target_slug)
        .join("messages")
        .join("source-message.md");
    assert!(
        !target_archive_file.exists(),
        "dry-run should not create target artifacts"
    );
}

#[test]
fn projects_adopt_dry_run_accepts_slug_identifiers() {
    let env = TestEnv::new();
    let (source_slug, target_slug, _source_key, _target_key) = seed_projects_for_adopt(&env, true);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_slug, &target_slug],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_adopt_dry_run_slug_identifiers",
            &["projects", "adopt", &source_slug, &target_slug],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Projects adopt plan (dry-run)"),
        "missing dry-run marker in stdout:\n{}",
        stdout
    );
    assert!(
        stdout.contains("- Source: id=1 slug=source-proj"),
        "expected source project plan line in stdout:\n{}",
        stdout
    );
    assert!(
        stdout.contains("- Target: id=2 slug=target-proj"),
        "expected target project plan line in stdout:\n{}",
        stdout
    );
}

#[test]
fn projects_adopt_apply_moves_artifacts_and_writes_aliases() {
    let env = TestEnv::new();
    let (source_slug, target_slug, source_key, target_key) = seed_projects_for_adopt(&env, true);
    let source_archive_file = env
        .storage_root
        .join("projects")
        .join(&source_slug)
        .join("messages")
        .join("source-message.md");
    std::fs::create_dir_all(source_archive_file.parent().expect("parent")).expect("create dir");
    std::fs::write(&source_archive_file, "hello").expect("write source archive file");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_key, &target_key, "--apply"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_adopt_apply",
            &["projects", "adopt", &source_key, &target_key, "--apply"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Adoption apply completed."),
        "missing completion marker in stdout:\n{}",
        stdout
    );
    assert!(
        !source_archive_file.exists(),
        "expected source artifact to be moved on --apply"
    );
    let target_archive_file = env
        .storage_root
        .join("projects")
        .join(&target_slug)
        .join("messages")
        .join("source-message.md");
    assert!(
        target_archive_file.exists(),
        "expected target artifact to exist on --apply"
    );

    let aliases_path = env
        .storage_root
        .join("projects")
        .join(&target_slug)
        .join("aliases.json");
    assert!(
        aliases_path.exists(),
        "expected aliases.json at {}",
        aliases_path.display()
    );
    let aliases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&aliases_path).expect("read aliases.json"))
            .expect("parse aliases.json");
    let former_slugs = aliases
        .get("former_slugs")
        .and_then(serde_json::Value::as_array)
        .expect("former_slugs array");
    assert!(
        former_slugs
            .iter()
            .any(|v| v.as_str() == Some(source_slug.as_str())),
        "expected source slug in former_slugs: {}",
        serde_json::to_string_pretty(&aliases).unwrap_or_else(|_| aliases.to_string())
    );
}

#[test]
fn projects_adopt_apply_cross_repo_refuses_and_keeps_source_artifacts() {
    let env = TestEnv::new();
    let (source_slug, target_slug, source_key, target_key) = seed_projects_for_adopt(&env, false);
    let source_archive_file = env
        .storage_root
        .join("projects")
        .join(&source_slug)
        .join("messages")
        .join("source-message.md");
    std::fs::create_dir_all(source_archive_file.parent().expect("parent")).expect("create dir");
    std::fs::write(&source_archive_file, "hello").expect("write source archive file");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_key, &target_key, "--apply"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_adopt_apply_cross_repo_refusal",
            &["projects", "adopt", &source_key, &target_key, "--apply"],
            &out,
        );
        panic!(
            "expected success with refusal semantics\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "Refusing to adopt: projects do not appear to belong to the same repository."
        ),
        "expected refusal message in stderr:\n{}",
        stderr
    );
    assert!(
        source_archive_file.exists(),
        "cross-repo refusal should keep source artifacts in place"
    );
    let target_archive_file = env
        .storage_root
        .join("projects")
        .join(&target_slug)
        .join("messages")
        .join("source-message.md");
    assert!(
        !target_archive_file.exists(),
        "cross-repo refusal should not create target artifacts"
    );
}

#[test]
fn projects_adopt_missing_source_exits_nonzero() {
    let env = TestEnv::new();
    let (_source_slug, _target_slug, _source_key, target_key) = seed_projects_for_adopt(&env, true);
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "projects",
            "adopt",
            "missing-source-slug",
            &target_key,
            "--apply",
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for missing project source\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("project not found"),
        "expected missing project error in stderr:\n{}",
        stderr
    );
}

#[test]
fn projects_adopt_same_project_is_noop_success() {
    let env = TestEnv::new();
    let (_source_slug, _target_slug, source_key, _target_key) = seed_projects_for_adopt(&env, true);
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_key, &source_key, "--apply"],
        None,
    );
    if !out.status.success() {
        write_artifact(
            "projects_adopt_same_project_noop",
            &["projects", "adopt", &source_key, &source_key, "--apply"],
            &out,
        );
        panic!(
            "expected success\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Source and target refer to the same project; nothing to do."),
        "expected no-op message in stdout:\n{}",
        stdout
    );
}

#[test]
fn projects_adopt_apply_duplicate_agent_name_conflict_exits_nonzero() {
    let env = TestEnv::new();
    let (source_slug, target_slug, source_key, target_key) = seed_projects_for_adopt(&env, true);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_agent(&conn, 101, 1, "GreenCastle", "test", "test");
    insert_agent(&conn, 202, 2, "greencastle", "test", "test");

    let source_archive_file = env
        .storage_root
        .join("projects")
        .join(&source_slug)
        .join("messages")
        .join("source-message.md");
    std::fs::create_dir_all(source_archive_file.parent().expect("parent")).expect("create dir");
    std::fs::write(&source_archive_file, "hello").expect("write source archive file");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["projects", "adopt", &source_key, &target_key, "--apply"],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for duplicate agent-name conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr
            .to_ascii_lowercase()
            .contains("agent name conflicts in target project"),
        "expected duplicate-agent conflict error in stderr:\n{}",
        stderr
    );
    assert!(
        source_archive_file.exists(),
        "conflict should preserve source artifacts"
    );
    let target_archive_file = env
        .storage_root
        .join("projects")
        .join(&target_slug)
        .join("messages")
        .join("source-message.md");
    assert!(
        !target_archive_file.exists(),
        "conflict should not move artifacts to target"
    );
}

// ---- Config commands ----

#[test]
fn config_show_port_prints_default() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["config", "show-port"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Default port from config; just ensure it prints a number
    let trimmed = stdout.trim();
    assert!(
        trimmed.parse::<u16>().is_ok(),
        "expected numeric port, got: {trimmed}"
    );
}

#[test]
fn config_set_port_creates_env_file() {
    let env = TestEnv::new();
    let env_path = env.tmp.path().join(".env");
    let env_path_str = env_path.to_string_lossy().to_string();

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["config", "set-port", "9876", "--env-file", &env_path_str],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Port set to 9876"),
        "expected port-set confirmation, got:\n{stdout}"
    );
    let body = std::fs::read_to_string(&env_path).expect("read .env");
    assert!(
        body.contains("HTTP_PORT=9876"),
        "expected canonical port in .env:\n{body}"
    );
    assert!(
        !body.contains("AGENT_MAIL_HTTP_PORT="),
        "legacy AGENT_MAIL_HTTP_PORT should not be written:\n{body}"
    );
}

#[test]
fn config_set_port_updates_existing_env_file() {
    let env = TestEnv::new();
    let env_path = env.tmp.path().join(".env");
    std::fs::write(
        &env_path,
        "SOME_VAR=foo\nAGENT_MAIL_HTTP_PORT=1111\nOTHER=bar\n",
    )
    .expect("write initial .env");
    let env_path_str = env_path.to_string_lossy().to_string();

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["config", "set-port", "5555", "--env-file", &env_path_str],
        None,
    );
    assert!(out.status.success(), "expected success");
    let body = std::fs::read_to_string(&env_path).expect("read .env");
    assert!(
        body.contains("HTTP_PORT=5555"),
        "expected updated canonical port in .env:\n{body}"
    );
    assert!(
        body.contains("SOME_VAR=foo"),
        "expected other vars preserved:\n{body}"
    );
    assert!(
        !body.contains("AGENT_MAIL_HTTP_PORT=1111"),
        "old port should be replaced:\n{body}"
    );
    assert!(
        !body.contains("AGENT_MAIL_HTTP_PORT="),
        "legacy AGENT_MAIL_HTTP_PORT should be removed:\n{body}"
    );
}

// ---- Doctor commands ----

#[test]
fn installed_binary_parity_report_flags_missing_forensic_timeline_fields() {
    let env = TestEnv::new();
    let source_outputs = json!({
        "doctor_check": {
            "healthy": true,
            "forensic_timeline": {
                "schema": "forensic-timeline.v1",
                "current": {
                    "binary_version": "0.2.52",
                    "schema_version": "12",
                    "storage_root": env.storage_root.display().to_string(),
                    "database_path": env.db_path.display().to_string(),
                    "search_index_generation": {
                        "state": "fresh"
                    }
                },
                "recent_recovery_runs": [],
                "artifacts": [],
                "next_actions": ["am doctor support-bundle --bearer-token super-secret"]
            }
        },
        "robot_status": {
            "forensic_timeline": {
                "schema": "forensic-timeline.v1",
                "current": {
                    "binary_version": "0.2.52",
                    "search_index_generation": {
                        "state": "fresh"
                    }
                }
            }
        },
        "robot_search": {
            "search_index": {
                "state": "fresh"
            }
        }
    });
    let installed_outputs = json!({
        "doctor_check": {
            "healthy": true,
            "forensic_timeline": null
        },
        "robot_status": {},
        "robot_search": {
            "search_index": {
                "state": "fresh"
            }
        }
    });

    let report = installed_binary_parity_report(
        &repo_root().join("target/debug/am"),
        Path::new("/usr/local/bin/am"),
        &env.storage_root,
        &env.database_url(),
        &source_outputs,
        &installed_outputs,
    );
    let run_root = installed_binary_parity_artifacts_dir().join(format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root).expect("create installed parity artifact root");
    write_json_artifact(&run_root, "missing_forensic_timeline_report.json", &report);

    assert_eq!(report["passed"], false);
    let checks = report["checks"].as_array().expect("checks array");
    assert!(
        checks.iter().any(|check| {
            check["surface"] == "doctor_check"
                && check["json_path"] == "forensic_timeline.schema"
                && check["status"] == "installed_missing"
        }),
        "report should flag missing installed doctor forensic timeline: {report}"
    );
    assert!(
        checks.iter().any(|check| {
            check["surface"] == "robot_status"
                && check["json_path"] == "forensic_timeline.schema"
                && check["status"] == "installed_missing"
        }),
        "report should flag missing installed robot forensic timeline: {report}"
    );
    let report_text = serde_json::to_string(&report).expect("serialize parity report");
    assert!(
        !report_text.contains("super-secret")
            && !report_text.contains(&env.storage_root.display().to_string())
            && !report_text.contains(&env.database_url()),
        "parity report must redact secrets and local paths: {report_text}"
    );
}

#[test]
fn doctor_support_bundle_adversarial_redaction_writes_report() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let forensics = env
        .storage_root
        .join("doctor")
        .join("forensics")
        .join("mailbox.sqlite3")
        .join("repair-20260512_225800_001");
    std::fs::create_dir_all(&forensics).expect("create forensic fixture");
    std::fs::write(
        forensics.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "command": "repair --bearer-token=manifest-command-secret",
            "safe_command": "am doctor support-bundle --bearer-token=manifest-safe-secret --database-url sqlite://user:pass@example.invalid/mail?token=query-secret",
            "env": {
                "HTTP_BEARER_TOKEN": "manifest-token",
                "OpenAi_Api_Key": "sk-manifest-key"
            },
            "subject": "Sensitive forensic subject",
            "body_md": "Private forensic body",
            "attachments": ["private-forensic-screenshot.png"],
            "artifact_path_kind": "raw_forensic_manifest",
            "reason_code": "foreign_key_integrity"
        }))
        .expect("serialize manifest fixture"),
    )
    .expect("write manifest fixture");
    std::fs::write(
        forensics.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "command": "repair --token=summary-command-secret",
            "nested": {
                "authorization": "Bearer summary-token",
                "content": "Private summary body"
            },
            "artifact_path_kind": "raw_forensic_summary",
            "reason_code": "repair_completed"
        }))
        .expect("serialize summary fixture"),
    )
    .expect("write summary fixture");

    let stdout_log = env.tmp.path().join("operator_stdout.log");
    let stderr_log = env.tmp.path().join("operator_stderr.log");
    std::fs::write(
        &stdout_log,
        format!(
            "Authorization: Bearer stdout-token\nTOKEN\u{ff1a}stdout-unicode-secret\nDATABASE_URL: sqlite://user:pass@example.invalid/mail\nsubject=Sensitive stdout subject\nbody_md=Private stdout body\nattachments=stdout-secret.png\npath={}\nsource_path_class=operator_supplied_log\ncommand_shape=am doctor support-bundle --json\n",
            env.db_path.display()
        ),
    )
    .expect("write stdout fixture");
    std::fs::write(
        &stderr_log,
        "safe_command=am doctor support-bundle --bearer-token=stderr-secret --json\nreason_code=operator_log\n",
    )
    .expect("write stderr fixture");

    let output_dir = env.tmp.path().join("support-output");
    let output_dir_text = output_dir.display().to_string();
    let stdout_log_text = stdout_log.display().to_string();
    let stderr_log_text = stderr_log.display().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "doctor",
            "support-bundle",
            "--output-dir",
            &output_dir_text,
            "--stdout-log",
            &stdout_log_text,
            "--stderr-log",
            &stderr_log_text,
            "--redact-subjects",
            "--json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "support-bundle should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let result: Value =
        serde_json::from_slice(&out.stdout).expect("support-bundle JSON should parse");
    let bundle_path = PathBuf::from(result["bundle_path"].as_str().expect("bundle_path string"));
    let mut bundle_text = String::new();
    let mut bundle_files = Vec::new();
    for entry in walkdir::WalkDir::new(&bundle_path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(&bundle_path)
            .expect("bundle file under bundle root")
            .display()
            .to_string();
        bundle_files.push(rel);
        bundle_text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        bundle_text.push('\n');
    }
    bundle_files.sort();
    let stdout_text = String::from_utf8_lossy(&out.stdout);
    let searchable = format!("{stdout_text}\n{bundle_text}");
    let forbidden = [
        (
            "manifest_command_secret",
            "manifest-command-secret".to_string(),
        ),
        ("manifest_safe_secret", "manifest-safe-secret".to_string()),
        ("query_secret", "query-secret".to_string()),
        ("manifest_token", "manifest-token".to_string()),
        ("manifest_api_key", "sk-manifest-key".to_string()),
        ("forensic_subject", "Sensitive forensic subject".to_string()),
        ("forensic_body", "Private forensic body".to_string()),
        (
            "forensic_attachment",
            "private-forensic-screenshot.png".to_string(),
        ),
        (
            "summary_command_secret",
            "summary-command-secret".to_string(),
        ),
        ("summary_token", "summary-token".to_string()),
        ("summary_body", "Private summary body".to_string()),
        ("stdout_token", "stdout-token".to_string()),
        ("stdout_unicode_secret", "stdout-unicode-secret".to_string()),
        ("stdout_subject", "Sensitive stdout subject".to_string()),
        ("stdout_body", "Private stdout body".to_string()),
        ("stdout_attachment", "stdout-secret.png".to_string()),
        ("stderr_secret", "stderr-secret".to_string()),
        ("database_credentials", "user:pass".to_string()),
        ("storage_root", env.storage_root.display().to_string()),
        ("database_path", env.db_path.display().to_string()),
    ];
    let leaked: Vec<&str> = forbidden
        .iter()
        .filter_map(|(label, value)| searchable.contains(value).then_some(*label))
        .collect();
    let retained = [
        ("raw_manifest_kind", "raw_forensic_manifest"),
        ("summary_kind", "raw_forensic_summary"),
        ("foreign_key_reason", "foreign_key_integrity"),
        ("operator_log_class", "operator_supplied_log"),
        ("command_shape", "am doctor support-bundle --json"),
        ("redacted_secret_marker", "<redacted-secret>"),
        ("redacted_body_marker", "<redacted-message-body>"),
        ("redacted_path_marker", "<redacted-path>"),
    ];
    let missing_retained: Vec<&str> = retained
        .iter()
        .filter_map(|(label, value)| (!searchable.contains(value)).then_some(*label))
        .collect();
    let forbidden_absent = leaked.is_empty();
    let retained_present = missing_retained.is_empty();

    let run_root = support_bundle_redaction_artifacts_dir().join(format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root).expect("create support bundle redaction artifact root");
    let report = json!({
        "schema": "support-bundle-redaction-adversarial.v1",
        "bead": "br-idea-wizard-swarm-reliability-2ac6x.8",
        "bundle_files": bundle_files,
        "forbidden_absent": forbidden_absent,
        "leaked_forbidden_labels": leaked.clone(),
        "retained_present": retained_present,
        "missing_retained_labels": missing_retained.clone(),
        "repro": "rch exec -- cargo test -p mcp-agent-mail-cli --test integration_runs doctor_support_bundle_adversarial_redaction_writes_report -- --nocapture"
    });
    write_json_artifact(&run_root, "redaction_report.json", &report);
    write_text_artifact(
        &run_root,
        "support_bundle_stdout.json",
        stdout_text.as_ref(),
    );
    write_text_artifact(
        &run_root,
        "support_bundle_stderr.txt",
        &String::from_utf8_lossy(&out.stderr),
    );
    eprintln!(
        "support bundle redaction artifact root: {}",
        run_root.display()
    );

    assert!(
        leaked.is_empty(),
        "support bundle leaked labels: {leaked:?}"
    );
    assert!(
        missing_retained.is_empty(),
        "support bundle dropped retained labels: {missing_retained:?}"
    );

    // N2 (br-bvq1x.14.2): the reliability snapshot must aggregate the
    // reliability-epic surfaces an incident responder needs in ONE file:
    // runtime identity (J2/J3), host pressure (J1), TUI/runtime loop
    // heartbeats (I1/I2), the unified process-owner model (I4), and the
    // fs-based mailbox-ownership/lock state (D1). Assert each section is
    // present and shaped, with its field_schema documented. HTTP_PORT=1 in
    // the test env makes the liveness/port probes deterministically
    // unreachable, so the snapshot is stable.
    let snapshot: Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_path.join("reports/reliability-snapshot.json"))
            .expect("reliability-snapshot.json present in bundle"),
    )
    .expect("reliability-snapshot.json parses");
    assert_eq!(
        snapshot["schema_version"], "1.1",
        "reliability snapshot schema_version bumped for the I1/I4/D1 sections"
    );
    for section in [
        "runtime_identity",
        "host",
        "tui_liveness",
        "process_owner",
        "process_owner_divergences",
        "mailbox_ownership",
    ] {
        assert!(
            snapshot.get(section).is_some(),
            "reliability snapshot missing section `{section}`: {snapshot:#}"
        );
        assert!(
            snapshot["field_schema"].get(section).is_some(),
            "reliability snapshot field_schema does not document `{section}`"
        );
    }
    // I1/I2: with the server unreachable (HTTP_PORT=1) the loop-liveness
    // probe records an honest `unreachable` source rather than fabricating
    // a verdict.
    assert_eq!(
        snapshot["tui_liveness"]["source"], "unreachable",
        "TUI liveness must report unreachable when no server is up"
    );
    // I4: the five process-owner dimensions are all present.
    for dim in ["expected_service", "actual_processes", "port", "db_path"] {
        assert!(
            snapshot["process_owner"].get(dim).is_some(),
            "process_owner missing dimension `{dim}`"
        );
    }
    assert!(
        snapshot["process_owner_divergences"].is_array(),
        "process_owner_divergences must be an array"
    );
    // D1: the fs-based lock surface exposes the disposition + lock paths.
    assert!(
        snapshot["mailbox_ownership"].get("disposition").is_some(),
        "mailbox_ownership missing disposition"
    );

    // The summary.json quick-triage projection surfaces the new at-a-glance
    // fields so triage does not have to open the full snapshot.
    let summary: Value = serde_json::from_str(
        &std::fs::read_to_string(bundle_path.join("summary.json"))
            .expect("summary.json present in bundle"),
    )
    .expect("summary.json parses");
    for quick_field in [
        "tui_liveness_overall",
        "process_owner_divergence_count",
        "supervisor_respawn_loop",
        "mailbox_ownership_disposition",
    ] {
        assert!(
            summary["reliability_snapshot"].get(quick_field).is_some(),
            "summary.reliability_snapshot missing quick-triage field `{quick_field}`"
        );
    }
}

#[test]
#[ignore = "release gate; set AM_INSTALLED_BINARY_PARITY_BIN to the installed am binary path"]
fn installed_binary_parity_probe_compares_source_and_installed_am() {
    let installed_binary = PathBuf::from(
        std::env::var("AM_INSTALLED_BINARY_PARITY_BIN")
            .expect("set AM_INSTALLED_BINARY_PARITY_BIN to the installed am binary path"),
    );
    assert!(
        installed_binary.is_file(),
        "installed am binary does not exist on this worker: {}",
        installed_binary.display()
    );
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(
        &conn,
        1,
        "installed-parity",
        &env.tmp.path().display().to_string(),
    );
    insert_agent(&conn, 1, 1, "Recipient", "codex-cli", "gpt-5");

    let source_outputs = collect_installed_parity_outputs(&am_bin(), &env);
    let installed_outputs = collect_installed_parity_outputs(&installed_binary, &env);
    let report = installed_binary_parity_report(
        &am_bin(),
        &installed_binary,
        &env.storage_root,
        &env.database_url(),
        &source_outputs,
        &installed_outputs,
    );

    let run_root = installed_binary_parity_artifacts_dir().join(format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root).expect("create installed parity artifact root");
    write_json_artifact(&run_root, "source_outputs.json", &source_outputs);
    write_json_artifact(&run_root, "installed_outputs.json", &installed_outputs);
    write_json_artifact(&run_root, "parity_report.json", &report);

    assert_eq!(
        report["passed"],
        true,
        "installed binary parity report failed; see {}: {}",
        run_root.display(),
        report
    );
}

#[test]
fn startup_recovery_crash_replay_writes_artifacts_and_smokes_repair_and_reconstruct() {
    let env = TestEnv::new();
    let env_vars = env.isolated_env();
    seed_startup_recovery_orphan_recipient(&env);

    let run_root = startup_recovery_artifacts_dir().join(format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root).expect("create startup recovery artifact root");
    eprintln!(
        "startup recovery crash replay artifact root: {}",
        run_root.display()
    );
    write_text_artifact(
        &run_root,
        "storage_root.txt",
        &format!(
            "storage_root={}\ndatabase_url={}\n",
            env.storage_root.display(),
            env.database_url()
        ),
    );
    write_text_artifact(
        &run_root,
        "repro.txt",
        "rch exec -- cargo test -p mcp-agent-mail-cli --test integration_runs \
         startup_recovery_crash_replay_writes_artifacts_and_smokes_repair_and_reconstruct -- --nocapture\n",
    );
    write_text_artifact(
        &run_root,
        "repro.env",
        "DATABASE_URL=<isolated temp sqlite fixture>\nSTORAGE_ROOT=<isolated temp storage root>\nHOME=<isolated temp home>\nXDG_CONFIG_HOME=<isolated temp config>\nAM_STARTUP_SEARCH_BACKFILL_DELAY_SECS=60 for deterministic reconstruct search transition\n",
    );

    let before = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &["doctor", "check", "--json"],
        None,
    );
    write_text_artifact(
        &run_root,
        "before_doctor.json",
        &String::from_utf8_lossy(&before.stdout),
    );
    write_text_artifact(
        &run_root,
        "before_doctor.stderr.txt",
        &String::from_utf8_lossy(&before.stderr),
    );
    let before_json: Value =
        serde_json::from_slice(&before.stdout).expect("before doctor JSON should parse");
    let before_text = serde_json::to_string(&before_json).expect("serialize before doctor JSON");
    assert!(
        before_text.contains("orphaned message_recipients")
            || before_text.contains("orphaned-recipient"),
        "before doctor output should identify the orphaned-recipient fixture:\n{before_text}"
    );

    let startup = run_startup_server_once(&env);
    write_text_artifact(&run_root, "startup_stdout.txt", &startup.stdout);
    write_text_artifact(&run_root, "startup_stderr.txt", &startup.stderr);
    let startup_combined = format!("{}\n{}", startup.stdout, startup.stderr);
    assert!(
        !startup.timed_out,
        "startup repair should finish before the harness timeout:\n{startup_combined}"
    );
    assert!(
        startup_combined.contains("Automatic mailbox repair completed"),
        "startup output should show automatic repair completion:\n{startup_combined}"
    );
    assert!(
        startup_combined.contains("Forensics:") || startup_combined.contains("forensic bundle:"),
        "startup output should preserve forensic artifact context:\n{startup_combined}"
    );
    assert!(
        !startup_combined.contains("RefCell already borrowed")
            && !startup_combined.contains("panicked at"),
        "startup replay must not panic after repair:\n{startup_combined}"
    );

    let after = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &["doctor", "check", "--json"],
        None,
    );
    assert!(
        after.status.success(),
        "after doctor check should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&after.stdout),
        String::from_utf8_lossy(&after.stderr)
    );
    write_text_artifact(
        &run_root,
        "after_doctor.json",
        &String::from_utf8_lossy(&after.stdout),
    );
    write_text_artifact(
        &run_root,
        "after_doctor.stderr.txt",
        &String::from_utf8_lossy(&after.stderr),
    );
    let after_json: Value =
        serde_json::from_slice(&after.stdout).expect("after doctor JSON should parse");
    assert_doctor_mailbox_recovered(&after_json, "after startup repair");

    let doctor_health = run_am(&env_vars, Some(env.tmp.path()), &["doctor", "health"], None);
    if !doctor_health.status.success() {
        assert!(
            doctor_non_environment_fail_checks(&after_json)
                .as_array()
                .is_some_and(Vec::is_empty),
            "doctor health failed after startup repair with mailbox failures\nstdout:\n{}\nstderr:\n{}\nnon-environment failures:\n{}",
            String::from_utf8_lossy(&doctor_health.stdout),
            String::from_utf8_lossy(&doctor_health.stderr),
            serde_json::to_string_pretty(&doctor_non_environment_fail_checks(&after_json)).unwrap()
        );
    }
    write_text_artifact(
        &run_root,
        "doctor_health.txt",
        &String::from_utf8_lossy(&doctor_health.stdout),
    );

    let robot_status = run_am(
        &env_vars,
        Some(env.tmp.path()),
        &[
            "robot",
            "status",
            "--agent",
            "Recipient",
            "--format",
            "json",
        ],
        None,
    );
    assert!(
        robot_status.status.success(),
        "robot status should succeed after startup repair\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&robot_status.stdout),
        String::from_utf8_lossy(&robot_status.stderr)
    );
    write_text_artifact(
        &run_root,
        "robot_status.json",
        &String::from_utf8_lossy(&robot_status.stdout),
    );
    let _: Value =
        serde_json::from_slice(&robot_status.stdout).expect("robot status JSON should parse");

    let stdio = run_stdio_session(&env_vars, &[tool_call(20, "health_check", json!({}))]);
    let stdio_artifact = json!({
        "stdout": stdio.stdout,
        "stderr": stdio.stderr,
        "exit_code": stdio.exit_code,
        "timed_out": stdio.timed_out,
        "responses": stdio.responses,
    });
    write_json_artifact(&run_root, "stdio_smoke.json", &stdio_artifact);
    let stdio_responses = stdio_artifact["responses"]
        .as_array()
        .expect("stdio responses array");
    let health_response = response_by_id(stdio_responses, 20).expect("stdio health_check response");
    assert!(
        !response_is_error(health_response),
        "stdio health_check should succeed: {health_response}"
    );

    let reconstruct_env = TestEnv::new();
    let mut reconstruct_env_vars = reconstruct_env.isolated_env();
    reconstruct_env_vars.push((
        "AM_STARTUP_SEARCH_BACKFILL_DELAY_SECS".to_string(),
        "60".to_string(),
    ));
    seed_startup_recovery_archive_only(&reconstruct_env);
    let reconstruct_project_path = reconstruct_env.tmp.path().join("reconstructed-project");
    write_text_artifact(
        &run_root,
        "reconstruct_storage_root.txt",
        &format!(
            "storage_root={}\ndatabase_url={}\n",
            reconstruct_env.storage_root.display(),
            reconstruct_env.database_url()
        ),
    );

    let reconstruct_before = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &["doctor", "check", "--json"],
        None,
    );
    write_text_artifact(
        &run_root,
        "reconstruct_before_doctor.json",
        &String::from_utf8_lossy(&reconstruct_before.stdout),
    );
    write_text_artifact(
        &run_root,
        "reconstruct_before_doctor.stderr.txt",
        &String::from_utf8_lossy(&reconstruct_before.stderr),
    );
    let reconstruct_before_json: Value = serde_json::from_slice(&reconstruct_before.stdout)
        .expect("reconstruct before doctor JSON should parse");
    let reconstruct_before_text =
        serde_json::to_string(&reconstruct_before_json).expect("serialize reconstruct before JSON");
    assert!(
        reconstruct_before_text.contains("Database file does not exist")
            && reconstruct_before_text.contains("doctor reconstruct --dry-run"),
        "before doctor output should identify the archive-backed reconstruct fixture:\n{reconstruct_before_text}"
    );

    let reconstruct_startup =
        run_startup_server_once_at_with_env(&reconstruct_env_vars, &reconstruct_project_path);
    write_text_artifact(
        &run_root,
        "reconstruct_startup_stdout.txt",
        &reconstruct_startup.stdout,
    );
    write_text_artifact(
        &run_root,
        "reconstruct_startup_stderr.txt",
        &reconstruct_startup.stderr,
    );
    let reconstruct_startup_combined = format!(
        "{}\n{}",
        reconstruct_startup.stdout, reconstruct_startup.stderr
    );
    assert!(
        reconstruct_startup_combined.contains("Automatic mailbox reconstruction completed"),
        "startup output should show automatic reconstruction completion:\n{reconstruct_startup_combined}"
    );
    assert!(
        !reconstruct_startup_combined.contains("RefCell already borrowed")
            && !reconstruct_startup_combined.contains("panicked at"),
        "startup reconstruct replay must not panic:\n{reconstruct_startup_combined}"
    );

    let reconstruct_after = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &["doctor", "check", "--json"],
        None,
    );
    assert!(
        reconstruct_after.status.success(),
        "after reconstruct doctor check should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&reconstruct_after.stdout),
        String::from_utf8_lossy(&reconstruct_after.stderr)
    );
    write_text_artifact(
        &run_root,
        "reconstruct_after_doctor.json",
        &String::from_utf8_lossy(&reconstruct_after.stdout),
    );
    write_text_artifact(
        &run_root,
        "reconstruct_after_doctor.stderr.txt",
        &String::from_utf8_lossy(&reconstruct_after.stderr),
    );
    let reconstruct_after_json: Value = serde_json::from_slice(&reconstruct_after.stdout)
        .expect("reconstruct after doctor JSON should parse");
    assert_doctor_mailbox_recovered(&reconstruct_after_json, "after startup reconstruct");

    let reconstruct_doctor_health = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &["doctor", "health"],
        None,
    );
    if !reconstruct_doctor_health.status.success() {
        assert!(
            doctor_non_environment_fail_checks(&reconstruct_after_json)
                .as_array()
                .is_some_and(Vec::is_empty),
            "doctor health failed after startup reconstruct with mailbox failures\nstdout:\n{}\nstderr:\n{}\nnon-environment failures:\n{}",
            String::from_utf8_lossy(&reconstruct_doctor_health.stdout),
            String::from_utf8_lossy(&reconstruct_doctor_health.stderr),
            serde_json::to_string_pretty(&doctor_non_environment_fail_checks(
                &reconstruct_after_json
            ))
            .unwrap()
        );
    }
    write_text_artifact(
        &run_root,
        "reconstruct_doctor_health.txt",
        &String::from_utf8_lossy(&reconstruct_doctor_health.stdout),
    );

    let reconstruct_robot_status = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &[
            "robot",
            "status",
            "--agent",
            "Recipient",
            "--format",
            "json",
        ],
        None,
    );
    assert!(
        reconstruct_robot_status.status.success(),
        "robot status should succeed after startup reconstruct\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&reconstruct_robot_status.stdout),
        String::from_utf8_lossy(&reconstruct_robot_status.stderr)
    );
    write_text_artifact(
        &run_root,
        "reconstruct_robot_status.json",
        &String::from_utf8_lossy(&reconstruct_robot_status.stdout),
    );
    let reconstruct_robot_status_json: Value =
        serde_json::from_slice(&reconstruct_robot_status.stdout)
            .expect("reconstruct robot status JSON should parse");
    let reconstruct_search_state_before = robot_search_index_state(
        &reconstruct_robot_status_json,
        "reconstruct robot status before search backfill",
    );
    assert_ne!(
        reconstruct_search_state_before, "fresh",
        "archive-only reconstruct should not claim a fresh Search V3 index before a real backfill"
    );

    let reconstruct_robot_search = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &[
            "robot",
            "search",
            "reconstruct",
            "--agent",
            "Recipient",
            "--format",
            "json",
        ],
        None,
    );
    assert!(
        reconstruct_robot_search.status.success(),
        "robot search should refresh Search V3 after startup reconstruct\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&reconstruct_robot_search.stdout),
        String::from_utf8_lossy(&reconstruct_robot_search.stderr)
    );
    write_text_artifact(
        &run_root,
        "reconstruct_robot_search.json",
        &String::from_utf8_lossy(&reconstruct_robot_search.stdout),
    );
    let reconstruct_robot_search_json: Value =
        serde_json::from_slice(&reconstruct_robot_search.stdout)
            .expect("reconstruct robot search JSON should parse");
    assert_eq!(
        robot_search_index_state(
            &reconstruct_robot_search_json,
            "reconstruct robot search after backfill"
        ),
        "fresh",
        "robot search should perform the real Search V3 lexical backfill"
    );
    assert!(
        robot_total_results(
            &reconstruct_robot_search_json,
            "reconstruct robot search after backfill"
        ) >= 1,
        "reconstruct robot search should find the archive-reconstructed message: {reconstruct_robot_search_json}"
    );

    let reconstruct_robot_status_after_search = run_am(
        &reconstruct_env_vars,
        Some(&reconstruct_project_path),
        &[
            "robot",
            "status",
            "--agent",
            "Recipient",
            "--format",
            "json",
        ],
        None,
    );
    assert!(
        reconstruct_robot_status_after_search.status.success(),
        "robot status should succeed after reconstruct search backfill\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&reconstruct_robot_status_after_search.stdout),
        String::from_utf8_lossy(&reconstruct_robot_status_after_search.stderr)
    );
    write_text_artifact(
        &run_root,
        "reconstruct_robot_status_after_search.json",
        &String::from_utf8_lossy(&reconstruct_robot_status_after_search.stdout),
    );
    let reconstruct_robot_status_after_search_json: Value =
        serde_json::from_slice(&reconstruct_robot_status_after_search.stdout)
            .expect("reconstruct robot status after search JSON should parse");
    assert_eq!(
        robot_search_index_state(
            &reconstruct_robot_status_after_search_json,
            "reconstruct robot status after search backfill"
        ),
        "fresh",
        "robot status should report fresh Search V3 health after the real backfill"
    );

    let reconstruct_stdio = run_stdio_session(
        &reconstruct_env_vars,
        &[tool_call(20, "health_check", json!({}))],
    );
    let reconstruct_stdio_artifact = json!({
        "stdout": reconstruct_stdio.stdout,
        "stderr": reconstruct_stdio.stderr,
        "exit_code": reconstruct_stdio.exit_code,
        "timed_out": reconstruct_stdio.timed_out,
        "responses": reconstruct_stdio.responses,
    });
    write_json_artifact(
        &run_root,
        "reconstruct_stdio_smoke.json",
        &reconstruct_stdio_artifact,
    );
    let reconstruct_stdio_responses = reconstruct_stdio_artifact["responses"]
        .as_array()
        .expect("reconstruct stdio responses array");
    let reconstruct_health_response = response_by_id(reconstruct_stdio_responses, 20)
        .expect("reconstruct stdio health_check response");
    assert!(
        !response_is_error(reconstruct_health_response),
        "reconstruct stdio health_check should succeed: {reconstruct_health_response}"
    );

    write_json_artifact(
        &run_root,
        "summary.json",
        &json!({
            "schema": "startup-recovery-crash-replay.v1",
            "bead": "br-idea-wizard-swarm-reliability-2ac6x.3",
            "startup": {
                "exit_code": startup.exit_code,
                "timed_out": startup.timed_out,
                "repair_completed": startup_combined.contains("Automatic mailbox repair completed"),
                "refcell_panic_seen": startup_combined.contains("RefCell already borrowed"),
            },
            "reconstruct_startup": {
                "exit_code": reconstruct_startup.exit_code,
                "timed_out": reconstruct_startup.timed_out,
                "reconstruct_completed": reconstruct_startup_combined.contains("Automatic mailbox reconstruction completed"),
                "refcell_panic_seen": reconstruct_startup_combined.contains("RefCell already borrowed"),
            },
            "doctor": {
                "before_healthy": before_json.get("healthy").and_then(Value::as_bool),
                "after_healthy": after_json.get("healthy").and_then(Value::as_bool),
                "doctor_health_exit_code": doctor_health.status.code(),
                "reconstruct_before_healthy": reconstruct_before_json.get("healthy").and_then(Value::as_bool),
                "reconstruct_after_healthy": reconstruct_after_json.get("healthy").and_then(Value::as_bool),
                "reconstruct_doctor_health_exit_code": reconstruct_doctor_health.status.code(),
            },
            "robot_status_exit_code": robot_status.status.code(),
            "reconstruct_robot_status_exit_code": reconstruct_robot_status.status.code(),
            "reconstruct_search_state_before": reconstruct_search_state_before,
            "reconstruct_robot_search_exit_code": reconstruct_robot_search.status.code(),
            "reconstruct_robot_status_after_search_exit_code": reconstruct_robot_status_after_search.status.code(),
            "reconstruct_search_state_after_search": robot_search_index_state(
                &reconstruct_robot_status_after_search_json,
                "summary reconstruct robot status after search backfill"
            ),
            "stdio_health_check_exit_code": stdio_artifact.get("exit_code"),
            "reconstruct_stdio_health_check_exit_code": reconstruct_stdio_artifact.get("exit_code"),
            "frankensqlite_dependency_note": {
                "local_source_path": "/data/projects/frankensqlite/crates/fsqlite-core/src/connection.rs",
                "borrow_boundary": "old schema fingerprint is scoped before self.schema.borrow_mut during reload",
                "binary_under_test": "cargo-built CARGO_BIN_EXE_am, not the installed release binary"
            }
        }),
    );
}

#[test]
fn doctor_check_on_migrated_db_passes() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "check"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Doctor check:"),
        "expected doctor check header:\n{stdout}"
    );
    assert!(
        stdout.contains("All checks passed."),
        "expected all checks passed:\n{stdout}"
    );
}

#[test]
fn doctor_check_and_list_projects_ignore_hostile_repo_dotenv_when_user_config_exists() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(
        &conn,
        1,
        "migrated-mailbox",
        "/Users/tester/projects/mcp_agent_mail",
    );

    env.write_user_config_env(&format!(
        "DATABASE_URL={}\nSTORAGE_ROOT={}\n",
        env.database_url(),
        env.storage_root.display()
    ));

    std::fs::write(
        env.hostile_repo().join(".env"),
        "DATABASE_URL=sqlite:///./storage.sqlite3\nSTORAGE_ROOT=./storage_root\n",
    )
    .expect("write hostile repo .env");
    std::fs::write(
        env.hostile_repo().join("storage.sqlite3"),
        b"this is not a sqlite database",
    )
    .expect("write hostile sqlite placeholder");

    let list_out = run_am_hermetic(
        &env.hermetic_env(),
        Some(env.hostile_repo()),
        &["list-projects", "--json"],
    );
    assert!(
        list_out.status.success(),
        "expected list-projects success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&list_out.stdout),
        String::from_utf8_lossy(&list_out.stderr)
    );
    let projects: serde_json::Value =
        serde_json::from_slice(&list_out.stdout).expect("valid list-projects JSON");
    let projects = projects.as_array().expect("project array");
    assert!(
        projects
            .iter()
            .any(|project| project.get("slug").and_then(|v| v.as_str()) == Some("migrated-mailbox")),
        "expected seeded migrated project, got:\n{}",
        serde_json::to_string_pretty(&projects).unwrap()
    );

    let doctor_out = run_am_hermetic(
        &env.hermetic_env(),
        Some(env.hostile_repo()),
        &["doctor", "check", "--json"],
    );
    assert!(
        doctor_out.status.success(),
        "expected doctor check success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&doctor_out.stdout),
        String::from_utf8_lossy(&doctor_out.stderr)
    );
    let doctor: serde_json::Value =
        serde_json::from_slice(&doctor_out.stdout).expect("valid doctor JSON");
    let checks = doctor["checks"].as_array().expect("checks array");
    let database_detail = checks
        .iter()
        .find(|check| check.get("check").and_then(|v| v.as_str()) == Some("database"))
        .and_then(|check| check.get("detail"))
        .and_then(|detail| detail.as_str())
        .expect("database detail");
    assert!(
        database_detail.contains(&env.db_path.display().to_string()),
        "doctor check did not use installer/user-config database:\n{database_detail}"
    );
    assert!(
        !database_detail.contains("./storage.sqlite3"),
        "doctor check incorrectly reported repo-local sqlite path:\n{database_detail}"
    );
}

#[test]
fn doctor_check_json_mode() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "check", "--json"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(
        value.get("healthy").and_then(|v| v.as_bool()),
        Some(true),
        "expected healthy=true in JSON"
    );
    assert!(
        value.get("summary").and_then(|v| v.as_object()).is_some(),
        "expected summary object"
    );
    let checks = value.get("checks").and_then(|v| v.as_array());
    assert!(checks.is_some(), "expected checks array");
    assert!(!checks.unwrap().is_empty(), "expected non-empty checks");
}

#[test]
fn doctor_read_only_commands_do_not_mutate_fixture_tree() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    mcp_agent_mail_db::pool::wal_checkpoint_truncate_path(&env.db_path)
        .expect("checkpoint read-only doctor fixture");

    let local_bin = env.home_dir.join(".local/bin");
    std::fs::create_dir_all(&local_bin).expect("create local bin");
    write_version_shim(&local_bin.join("am"), "am");
    write_version_shim(&local_bin.join("mcp-agent-mail"), "mcp-agent-mail");

    let mut env_vars = env.isolated_env();
    env_vars.retain(|(key, _)| key != "PATH");
    env_vars.push((
        "PATH".to_string(),
        format!("{}:/usr/local/bin:/usr/bin:/bin", local_bin.display()),
    ));

    let before = snapshot_tree(env.tmp.path());
    for args in [
        &["doctor", "health"][..],
        &["doctor", "check"][..],
        &["doctor", "check", "--json"][..],
    ] {
        let out = run_am_hermetic(&env_vars, Some(env.tmp.path()), args);
        assert!(
            out.status.success(),
            "expected read-only doctor command to succeed for {args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if args == &["doctor", "check", "--json"][..] {
            let value: serde_json::Value =
                serde_json::from_slice(&out.stdout).expect("valid doctor JSON");
            assert_eq!(
                value.get("healthy").and_then(|v| v.as_bool()),
                Some(true),
                "expected healthy=true in JSON"
            );
        }

        let after = snapshot_tree(env.tmp.path());
        assert_eq!(
            before, after,
            "read-only doctor command mutated the fixture tree: {args:?}"
        );
    }
}

#[test]
fn doctor_check_verbose_shows_details() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "check", "--verbose"],
        None,
    );
    assert!(out.status.success(), "expected success");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Primary issue:") || stdout.contains("Summary:"),
        "expected operator summary block:\n{stdout}"
    );
    // Verbose mode shows details after the check name
    assert!(
        stdout.contains("SQLite database accessible") || stdout.contains(" - "),
        "expected verbose detail output:\n{stdout}"
    );
}

#[test]
fn doctor_backups_empty_returns_success() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "backups"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No backups found"),
        "expected empty backups message:\n{stdout}"
    );
}

#[test]
fn doctor_backups_json_empty_returns_array() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "backups", "--json"],
        None,
    );
    assert!(out.status.success(), "expected success");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(value.is_array(), "expected JSON array");
    assert!(
        value.as_array().unwrap().is_empty(),
        "expected empty array for no backups"
    );
}

// ---- Mail status ----

#[test]
fn mail_status_on_seeded_project() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    let project_path = env.tmp.path().join("mail_proj");
    std::fs::create_dir_all(&project_path).expect("create project dir");
    let project_path_str = project_path.to_string_lossy().to_string();
    insert_project(&conn, 1, "mail-proj", &project_path_str);
    insert_agent(&conn, 1, 1, "GoldHawk", "test", "test");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["mail", "status", &project_path_str],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Messages") && stdout.contains("Agents"),
        "expected status output with Messages and Agents:\n{stdout}"
    );
}

// ---- File Reservations ----

#[test]
fn file_reservations_list_on_migrated_db() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "fr-list-proj", "/tmp/fr-list-proj");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["file_reservations", "list", "fr-list-proj"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn file_reservations_active_with_seeded_data() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "res-proj", "/tmp/res-proj");
    insert_agent(&conn, 1, 1, "RedLake", "test", "test");
    // Reservation expiring far in the future
    let far_future = 4_102_444_800_000_000i64; // ~2100-01-01
    insert_file_reservation(&conn, 1, 1, 1, "src/*.rs", true, far_future);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["file_reservations", "active", "res-proj"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("src/*.rs") || stdout.contains("RedLake"),
        "expected reservation data in output:\n{stdout}"
    );
}

#[test]
fn file_reservations_soon_returns_expiring() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "soon-proj", "/tmp/soon-proj");
    insert_agent(&conn, 1, 1, "BlueFox", "test", "test");
    // Reservation expiring in 5 minutes from now
    let five_min_from_now = mcp_agent_mail_db::timestamps::now_micros() + 5 * 60 * 1_000_000;
    insert_file_reservation(&conn, 1, 1, 1, "data/*.json", true, five_min_from_now);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["file_reservations", "soon", "soon-proj", "--minutes", "10"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("data/*.json") || stdout.contains("BlueFox"),
        "expected expiring reservation in output:\n{stdout}"
    );
}

// ---- Acks ----

#[test]
fn acks_pending_empty_db() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "ack-pend", "/tmp/ack-pend");
    insert_agent(&conn, 1, 1, "GoldFox", "test", "test");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["acks", "pending", "ack-pend", "GoldFox"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn acks_overdue_empty_db() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "ack-over", "/tmp/ack-over");
    insert_agent(&conn, 1, 1, "GoldFox", "test", "test");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["acks", "overdue", "ack-over", "GoldFox"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn list_acks_with_seeded_project() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "ack-proj", "/tmp/ack-proj");
    insert_agent(&conn, 1, 1, "GoldFox", "test", "test");
    insert_agent(&conn, 2, 1, "RedLake", "test", "test");
    // Insert a message with ack_required from RedLake to GoldFox
    insert_message(
        &conn,
        1,
        1,
        2,
        "Need your review",
        "Please review the plan.",
    );
    insert_recipient(&conn, 1, 1);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["list-acks", "--project", "ack-proj", "--agent", "GoldFox"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn robot_handoff_dashboard_writes_artifacts_and_keeps_beads_read_only() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    let project_path = env.tmp.path().join("handoff_project");
    let beads_dir = project_path.join(".beads");
    std::fs::create_dir_all(&beads_dir).expect("create beads dir");
    let project_path_str = project_path.to_string_lossy().to_string();
    insert_project(&conn, 1, "handoff-proj", &project_path_str);
    insert_agent(&conn, 1, 1, "ActiveAgent", "codex-cli", "gpt-5");
    insert_agent(&conn, 2, 1, "ReservedAgent", "codex-cli", "gpt-5");
    insert_agent(&conn, 3, 1, "AckAgent", "codex-cli", "gpt-5");
    insert_agent(&conn, 4, 1, "InactiveAgent", "codex-cli", "gpt-5");
    insert_agent(&conn, 5, 1, "FreshAgent", "codex-cli", "gpt-5");

    let now_us = mcp_agent_mail_db::timestamps::now_micros();
    conn.execute_sync(
        "UPDATE agents SET last_active_ts = ? WHERE name = ?",
        &[
            SqlValue::BigInt(now_us),
            SqlValue::Text("ActiveAgent".to_string()),
        ],
    )
    .expect("mark active agent fresh");
    insert_file_reservation(
        &conn,
        1,
        1,
        2,
        "crates/mcp-agent-mail-cli/src/robot.rs",
        true,
        now_us + 2 * 60 * 60 * 1_000_000,
    );
    conn.execute_sync(
        "INSERT INTO messages (\
            id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts\
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(900),
            SqlValue::BigInt(1),
            SqlValue::BigInt(1),
            SqlValue::Text("br-handoff-ack".to_string()),
            SqlValue::Text("Ack needed".to_string()),
            SqlValue::Text("Please confirm handoff state.".to_string()),
            SqlValue::Text("normal".to_string()),
            SqlValue::Bool(true),
            SqlValue::BigInt(now_us - 2 * 60 * 60 * 1_000_000),
        ],
    )
    .expect("insert ack message");
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, ?, ?, ?)",
        &[
            SqlValue::BigInt(900),
            SqlValue::BigInt(3),
            SqlValue::Text("to".to_string()),
            SqlValue::Null,
            SqlValue::Null,
        ],
    )
    .expect("insert ack recipient");

    let stale_at = mcp_agent_mail_db::micros_to_iso(now_us - 48 * 60 * 60 * 1_000_000);
    let fresh_at = mcp_agent_mail_db::micros_to_iso(now_us - 5 * 60 * 1_000_000);
    let issues_jsonl = [
        json!({
            "id": "br-handoff-active",
            "title": "Active owner should keep",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": [{"author": "ActiveAgent", "text": "still on it", "created_at": stale_at}]
        }),
        json!({
            "id": "br-handoff-reserved",
            "title": "Reservation should block",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": [{"author": "ReservedAgent", "text": "holding files", "created_at": stale_at}]
        }),
        json!({
            "id": "br-handoff-ack",
            "title": "Ack-required mail should ask owner",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": [{"author": "AckAgent", "text": "waiting", "created_at": stale_at}]
        }),
        json!({
            "id": "br-handoff-inactive",
            "title": "Inactive owner can be taken over",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": [{"author": "InactiveAgent", "text": "started", "created_at": stale_at}]
        }),
        json!({
            "id": "br-handoff-no-owner",
            "title": "No owner can be reopened",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": []
        }),
        json!({
            "id": "br-handoff-fresh-comment",
            "title": "Fresh comment should keep",
            "status": "in_progress",
            "updated_at": stale_at,
            "comments": [{"author": "FreshAgent", "text": "fresh handoff note", "created_at": fresh_at}]
        }),
    ]
    .into_iter()
    .map(|value| serde_json::to_string(&value).expect("serialize issue"))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let issues_path = beads_dir.join("issues.jsonl");
    std::fs::write(&issues_path, &issues_jsonl).expect("write handoff issues");
    let before = std::fs::read_to_string(&issues_path).expect("read before issues");

    let out = run_am(
        &env.base_env(),
        Some(&project_path),
        &[
            "robot",
            "handoff",
            "--project",
            &project_path_str,
            "--include-fresh",
            "--dry-run",
            "--stale-minutes",
            "60",
            "--active-minutes",
            "30",
            "--fresh-comment-minutes",
            "30",
            "--format",
            "json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "robot handoff should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).expect("valid handoff JSON");
    let records = report["records"].as_array().expect("records array");
    let action_for = |id: &str| {
        records
            .iter()
            .find(|record| record["id"].as_str() == Some(id))
            .and_then(|record| record["action"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let safe_command_for = |id: &str| {
        records
            .iter()
            .find(|record| record["id"].as_str() == Some(id))
            .and_then(|record| record["safe_command"].as_str())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(action_for("br-handoff-active"), "keep");
    assert_eq!(action_for("br-handoff-reserved"), "blocked_by_reservation");
    assert_eq!(action_for("br-handoff-ack"), "ask_owner");
    assert_eq!(action_for("br-handoff-inactive"), "takeover_candidate");
    assert_eq!(action_for("br-handoff-no-owner"), "reopen_candidate");
    assert_eq!(action_for("br-handoff-fresh-comment"), "keep");
    assert_eq!(
        report["dry_run"].as_bool(),
        Some(true),
        "handoff command must report dry-run/read-only mode"
    );
    assert!(
        records.iter().any(|record| {
            record["safe_command"].as_str().is_some_and(|command| {
                command.starts_with("cd ")
                    && command.contains(&project_path_str)
                    && command.contains("br update br-handoff-no-owner --status open --json")
            })
        }),
        "expected cwd-anchored proposed reopen command in report:\n{}",
        serde_json::to_string_pretty(&report).unwrap()
    );
    let takeover_command = safe_command_for("br-handoff-inactive");
    assert!(
        takeover_command.starts_with("cd ")
            && takeover_command.contains(&project_path_str)
            && takeover_command.contains("br update br-handoff-inactive --claim --json"),
        "expected cwd-anchored proposed claim command, got: {takeover_command}"
    );
    let after = std::fs::read_to_string(&issues_path).expect("read after issues");
    assert_eq!(after, before, "robot handoff must not mutate beads JSONL");

    let run_root = stale_handoff_artifacts_dir().join(format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root).expect("create handoff artifact root");
    write_json_artifact(&run_root, "handoff_report.json", &report);
    write_text_artifact(
        &run_root,
        "stdout.json",
        &String::from_utf8_lossy(&out.stdout),
    );
    write_text_artifact(
        &run_root,
        "stderr.txt",
        &String::from_utf8_lossy(&out.stderr),
    );
    write_text_artifact(&run_root, "issues_before.jsonl", &before);
    write_text_artifact(
        &run_root,
        "repro.txt",
        "rch exec -- cargo test -p mcp-agent-mail-cli --test integration_runs robot_handoff_dashboard_writes_artifacts_and_keeps_beads_read_only -- --nocapture\n",
    );
    eprintln!(
        "stale handoff dashboard artifact root: {}",
        run_root.display()
    );
}

// ---- Amctl ----

#[test]
fn amctl_env_shows_variables() {
    let env = TestEnv::new();
    let project = env.tmp.path().join("amctl_proj");
    init_git_repo(&project);

    let project_str = project.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["amctl", "env", "-p", &project_str],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SLUG="),
        "expected SLUG= in output:\n{stdout}"
    );
    assert!(
        stdout.contains("PROJECT_UID="),
        "expected PROJECT_UID= in output:\n{stdout}"
    );
    assert!(
        stdout.contains("BRANCH="),
        "expected BRANCH= in output:\n{stdout}"
    );
    assert!(
        stdout.contains("AGENT="),
        "expected AGENT= in output:\n{stdout}"
    );
    assert!(
        stdout.contains("CACHE_KEY="),
        "expected CACHE_KEY= in output:\n{stdout}"
    );
    assert!(
        stdout.contains("ARTIFACT_DIR="),
        "expected ARTIFACT_DIR= in output:\n{stdout}"
    );
}

// ---- Clear and reset ----

#[test]
fn clear_and_reset_refuses_without_force_on_non_interactive() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["clear-and-reset-everything"],
        None,
    );
    assert!(
        !out.status.success(),
        "expected failure without --force on non-interactive stdin"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--force") || stderr.contains("non-interactive"),
        "expected force-required error in stderr:\n{stderr}"
    );
}

#[test]
fn clear_and_reset_with_force_and_no_archive_succeeds() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["clear-and-reset-everything", "--force", "--no-archive"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success with --force --no-archive\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // After reset, the database file should be removed
    assert!(
        !env.db_path.exists(),
        "expected database to be removed after reset"
    );
}

// ---- Archive commands ----

#[test]
fn archive_list_json_empty_returns_array() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["archive", "list", "--json"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Either empty JSON array or message about no archives
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
        assert!(value.is_array(), "expected JSON array");
    }
}

#[test]
fn archive_save_and_list_roundtrip() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    // Archive save requires the storage root to exist and at least one project
    std::fs::create_dir_all(&env.storage_root).expect("create storage root");
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "archive-proj", "/tmp/archive-proj");

    // Save archive
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["archive", "save", "--label", "test-snapshot"],
        None,
    );
    assert!(
        out.status.success(),
        "expected save success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // List archives
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["archive", "list", "--json"],
        None,
    );
    assert!(
        out.status.success(),
        "expected list success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let arr = value.as_array().expect("JSON array");
    assert!(
        !arr.is_empty(),
        "expected at least one archive after save, got empty array"
    );
}

// ---- Share commands ----

#[test]
fn share_export_dry_run_succeeds() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "export-proj", "/tmp/export-proj");
    insert_agent(&conn, 1, 1, "GoldHawk", "test", "test");

    let output_dir = env.tmp.path().join("export_output");
    let output_str = output_dir.to_string_lossy().to_string();

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["share", "export", "-o", &output_str, "--dry-run"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success for dry-run export\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn share_verify_on_nonexistent_bundle_fails() {
    let env = TestEnv::new();
    let bundle = env.tmp.path().join("nonexistent-bundle");

    let bundle_str = bundle.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["share", "verify", &bundle_str],
        None,
    );
    assert!(
        !out.status.success(),
        "expected failure for nonexistent bundle"
    );
}

// ---- Docs commands ----

#[test]
fn docs_insert_blurbs_dry_run_scans_without_modifying() {
    let env = TestEnv::new();
    let scan_dir = env.tmp.path().join("docs_scan");
    std::fs::create_dir_all(&scan_dir).expect("create scan dir");

    // Create a markdown file with a blurb marker
    let md_file = scan_dir.join("test.md");
    std::fs::write(
        &md_file,
        "# Title\n\nSome content.\n\n<!-- am:blurb -->\n\nMore content.\n",
    )
    .expect("write markdown");

    let scan_dir_str = scan_dir.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "docs",
            "insert-blurbs",
            "--scan-dir",
            &scan_dir_str,
            "--dry-run",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Scanned"),
        "expected 'Scanned' in output:\n{stdout}"
    );
    assert!(
        stdout.contains("(dry run)"),
        "expected '(dry run)' marker:\n{stdout}"
    );
    // Verify file not modified
    let content = std::fs::read_to_string(&md_file).expect("read md");
    assert!(
        !content.contains("am:blurb:end"),
        "dry-run should not insert end markers"
    );
}

#[test]
fn docs_insert_blurbs_applies_end_markers() {
    let env = TestEnv::new();
    let scan_dir = env.tmp.path().join("docs_apply");
    std::fs::create_dir_all(&scan_dir).expect("create scan dir");

    let md_file = scan_dir.join("apply.md");
    std::fs::write(&md_file, "# Title\n\n<!-- am:blurb -->\n\nContent here.\n")
        .expect("write markdown");

    let scan_dir_str = scan_dir.to_string_lossy().to_string();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &[
            "docs",
            "insert-blurbs",
            "--scan-dir",
            &scan_dir_str,
            "--yes",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(&md_file).expect("read md after apply");
    assert!(
        content.contains("<!-- am:blurb:end -->"),
        "expected end marker inserted:\n{content}"
    );
}

// ---- List projects ----

#[test]
fn list_projects_with_agents_shows_agent_names() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);
    let conn = mcp_agent_mail_db::DbConn::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_project(&conn, 1, "agent-proj", "/tmp/agent-proj");
    insert_agent(&conn, 1, 1, "GoldHawk", "test", "test");

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["list-projects", "--include-agents", "--json"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(value.is_array(), "expected JSON array");
    let arr = value.as_array().unwrap();
    assert!(!arr.is_empty(), "expected at least one project");
    // Check that agent info is present
    let project = &arr[0];
    assert!(
        project.get("agents").is_some(),
        "expected agents field with --include-agents"
    );
}

// ---- Serve commands (dry checks) ----

#[test]
fn legacy_am_serve_reports_migration_preflight() {
    let env = TestEnv::new();
    let out = run_am(&env.base_env(), Some(env.tmp.path()), &["serve"], None);

    assert_eq!(
        out.status.code(),
        Some(mcp_agent_mail_cli::LEGACY_AM_SERVE_EXIT_CODE),
        "legacy am serve should use the dedicated migration exit code\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "legacy am serve preflight must not write stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    for fragment in [
        "legacy `am serve` is retired",
        "classification: legacy-subcommand-migration",
        "retry_policy: do-not-retry-unchanged",
        "am serve-http",
        "am serve-stdio",
        "mcp-agent-mail serve",
    ] {
        assert!(
            stderr.contains(fragment),
            "legacy am serve stderr missing {fragment:?}\nActual stderr:\n{stderr}"
        );
    }
}

#[test]
fn serve_http_help_exits_zero() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["serve-http", "--help"],
        None,
    );
    assert!(
        out.status.success(),
        "expected help success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--host") || stdout.contains("--port"),
        "expected serve-http help to mention --host/--port"
    );
}

#[test]
fn serve_stdio_help_exits_zero() {
    let env = TestEnv::new();
    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["serve-stdio", "--help"],
        None,
    );
    assert!(
        out.status.success(),
        "expected help success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- Doctor repair dry-run ----

#[test]
fn doctor_repair_dry_run_exits_zero() {
    let env = TestEnv::new();
    init_cli_schema(&env.db_path);

    let out = run_am(
        &env.base_env(),
        Some(env.tmp.path()),
        &["doctor", "repair", "--dry-run", "--yes"],
        None,
    );
    assert!(
        out.status.success(),
        "expected success for doctor repair --dry-run\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
