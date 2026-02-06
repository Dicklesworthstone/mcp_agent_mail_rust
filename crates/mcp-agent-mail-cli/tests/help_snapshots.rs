#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

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
    repo_root().join("tests/fixtures/cli_help")
}

fn artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/cli/help")
}

fn normalize_help(mut s: String) -> String {
    // Normalize newlines and trim trailing whitespace per line.
    s = s.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn read_fixture(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn write_fixture(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture dir");
    }
    std::fs::write(path, contents).expect("write fixture");
}

fn write_artifact(case: &str, contents: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let pid = std::process::id();
    let dir = artifacts_dir().join(format!("{ts}_{pid}"));
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    let path = dir.join(format!("{case}.txt"));
    std::fs::write(&path, contents).expect("write artifact");
    eprintln!("help snapshot mismatch saved to {}", path.display());
}

fn unified_diff(expected: &str, actual: &str) -> String {
    let diff = similar::TextDiff::from_lines(expected, actual);
    diff.unified_diff().header("expected", "actual").to_string()
}

fn run_help(args: &[&str]) -> String {
    let out = Command::new(am_bin())
        .args(args)
        // Help output should not depend on TTY; keep wrapping stable.
        .env("COLUMNS", "120")
        .output()
        .expect("failed to spawn am");

    assert!(
        out.status.success(),
        "expected success for args={args:?}, got status={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // clap typically prints help to stdout; fall back to stderr for safety.
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        stdout.to_string()
    } else {
        String::from_utf8_lossy(&out.stderr).to_string()
    }
}

fn assert_help_snapshot(case: &str, args: &[&str]) {
    let actual_raw = run_help(args);
    let actual = normalize_help(actual_raw);
    let fixture_path = fixtures_dir().join(format!("{case}.txt"));

    let update = std::env::var("UPDATE_CLI_HELP_SNAPSHOTS")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();

    match read_fixture(&fixture_path) {
        Some(expected_raw) => {
            let expected = normalize_help(expected_raw);
            if expected == actual {
                return;
            }

            if update {
                write_fixture(&fixture_path, &actual);
                return;
            }

            write_artifact(case, &actual);
            let diff = unified_diff(&expected, &actual);
            panic!(
                "help snapshot mismatch for {case} ({args:?})\n\
                 Hint: set UPDATE_CLI_HELP_SNAPSHOTS=1 to update fixtures\n\n{diff}"
            );
        }
        None => {
            if update {
                write_fixture(&fixture_path, &actual);
                return;
            }
            write_artifact(case, &actual);
            panic!(
                "missing help fixture {path}\n\
                 Hint: generate fixtures with UPDATE_CLI_HELP_SNAPSHOTS=1",
                path = fixture_path.display()
            );
        }
    }
}

#[test]
fn cli_help_snapshots() {
    // br-2ei.5.7.3: golden help snapshots
    let cases: &[(&str, &[&str])] = &[
        ("top_level", &["--help"]),
        ("serve_http", &["serve-http", "--help"]),
        ("serve_stdio", &["serve-stdio", "--help"]),
        ("share", &["share", "--help"]),
        ("share_export", &["share", "export", "--help"]),
        ("guard", &["guard", "--help"]),
        ("guard_check", &["guard", "check", "--help"]),
        ("doctor", &["doctor", "--help"]),
        ("doctor_repair", &["doctor", "repair", "--help"]),
        ("archive", &["archive", "--help"]),
        ("products", &["products", "--help"]),
        ("docs", &["docs", "--help"]),
        ("docs_insert_blurbs", &["docs", "insert-blurbs", "--help"]),
        ("migrate", &["migrate", "--help"]),
        (
            "clear_and_reset_everything",
            &["clear-and-reset-everything", "--help"],
        ),
        ("amctl", &["amctl", "--help"]),
        ("amctl_env", &["amctl", "env", "--help"]),
        ("am_run", &["am-run", "--help"]),
        ("projects", &["projects", "--help"]),
    ];

    for (case, args) in cases {
        assert_help_snapshot(case, args);
    }
}
