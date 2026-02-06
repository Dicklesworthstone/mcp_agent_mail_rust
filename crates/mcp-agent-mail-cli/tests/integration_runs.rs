#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
