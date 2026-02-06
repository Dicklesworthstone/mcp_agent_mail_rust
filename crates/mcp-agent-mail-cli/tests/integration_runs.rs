#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mcp_agent_mail_db::sqlmodel::Value as SqlValue;

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
            // Guard check requires this.
            ("AGENT_NAME".to_string(), "RusticGlen".to_string()),
            // Avoid accidental network calls: force HTTP tool paths to fail fast.
            ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
            ("HTTP_PORT".to_string(), "1".to_string()),
            ("HTTP_PATH".to_string(), "/mcp/".to_string()),
        ]
    }
}

fn run_am(
    env: &[(String, String)],
    cwd: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Output {
    let mut cmd = Command::new(am_bin());
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
        child.wait_with_output().expect("wait for am output")
    } else {
        cmd.output().expect("spawn am")
    }
}

fn init_cli_schema(db_path: &Path) {
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path.display().to_string())
        .expect("open sqlite db");
    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql())
        .expect("init schema");
}

fn insert_project(conn: &sqlmodel_sqlite::SqliteConnection, id: i64, slug: &str, human_key: &str) {
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
    conn: &sqlmodel_sqlite::SqliteConnection,
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

fn insert_recipient(conn: &sqlmodel_sqlite::SqliteConnection, message_id: i64, agent_id: i64) {
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

fn insert_file_reservation(
    conn: &sqlmodel_sqlite::SqliteConnection,
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
    conn: &sqlmodel_sqlite::SqliteConnection,
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
        .args(["init"])
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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

#[test]
fn migrate_then_list_projects_json_smoke() {
    let env = TestEnv::new();

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
fn guard_install_status_uninstall_smoke() {
    let env = TestEnv::new();
    let repo = env.tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    // Guard expects a git repo (hooks dir lives under .git/hooks by default).
    let git = Command::new("git")
        .current_dir(&repo)
        .args(["init"])
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
        .expect("open sqlite db");
    insert_agent(&conn, 101, 1, "GreenCastle", "test", "test");
    insert_agent(&conn, 202, 2, "GreenCastle", "test", "test");

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
        body.contains("AGENT_MAIL_HTTP_PORT=9876"),
        "expected port in .env:\n{body}"
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
        body.contains("AGENT_MAIL_HTTP_PORT=5555"),
        "expected updated port in .env:\n{body}"
    );
    assert!(
        body.contains("SOME_VAR=foo"),
        "expected other vars preserved:\n{body}"
    );
    assert!(
        !body.contains("AGENT_MAIL_HTTP_PORT=1111"),
        "old port should be replaced:\n{body}"
    );
}

// ---- Doctor commands ----

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
    let checks = value.get("checks").and_then(|v| v.as_array());
    assert!(checks.is_some(), "expected checks array");
    assert!(!checks.unwrap().is_empty(), "expected non-empty checks");
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(env.db_path.display().to_string())
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
