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
