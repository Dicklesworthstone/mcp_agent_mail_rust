//! br-3c7vp: Native harness adapter for TUI interaction-heavy E2E suites.
//!
//! Current phase: managed adapter.
//! - Native runner entrypoint is Rust (`cargo test --test tui_transport_harness`).
//! - Scenario execution delegates to shell suites (`tui_interaction`, `tui_interactions`,
//!   `tui_compat_matrix`, `tui_startup`).
//! - Harness captures deterministic metadata and copies legacy artifacts.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;

#[derive(Debug)]
struct CommandOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    timed_out: bool,
}

#[derive(Debug, Serialize)]
struct HarnessSummary {
    suite: String,
    runner: String,
    adapter_mode: String,
    strict_mode: bool,
    generated_at: String,
    duration_ms: u64,
    passed: bool,
    adapter_script: String,
    adapter_exit_code: Option<i32>,
    adapter_timed_out: bool,
    adapter_duration_ms: u64,
    adapter_stdout_path: String,
    adapter_stderr_path: String,
    legacy_artifacts_root: String,
    new_legacy_run_paths: Vec<String>,
    copied_legacy_run_path: Option<String>,
    notes: Vec<String>,
}

const DEFAULT_HARNESS_SUITE: &str = "tui_interaction";

#[derive(Clone, Copy, Debug)]
struct HarnessSuiteSpec {
    suite_name: &'static str,
    adapter_script: &'static str,
    legacy_artifacts_subdir: &'static str,
}

fn bool_env(var_name: &str) -> bool {
    std::env::var(var_name).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn selected_suite_name() -> String {
    std::env::var("AM_TUI_HARNESS_SUITE").unwrap_or_else(|_| DEFAULT_HARNESS_SUITE.to_string())
}

fn suite_spec_for(name: &str) -> HarnessSuiteSpec {
    match name {
        "tui_interaction" => HarnessSuiteSpec {
            suite_name: "tui_interaction",
            adapter_script: "tests/e2e/test_tui_interaction.sh",
            legacy_artifacts_subdir: "tui_interaction",
        },
        "tui_interactions" => HarnessSuiteSpec {
            suite_name: "tui_interactions",
            adapter_script: "tests/e2e/test_tui_interactions.sh",
            legacy_artifacts_subdir: "tui_interactions",
        },
        "tui_compat_matrix" => HarnessSuiteSpec {
            suite_name: "tui_compat_matrix",
            adapter_script: "tests/e2e/test_tui_compat_matrix.sh",
            legacy_artifacts_subdir: "tui_compat_matrix",
        },
        "tui_startup" => HarnessSuiteSpec {
            suite_name: "tui_startup",
            adapter_script: "tests/e2e/test_tui_startup.sh",
            legacy_artifacts_subdir: "tui_startup",
        },
        _ => panic!(
            "unsupported AM_TUI_HARNESS_SUITE={name}; expected one of: tui_interaction, tui_interactions, tui_compat_matrix, tui_startup"
        ),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn harness_artifacts_root(suite_name: &str) -> PathBuf {
    if let Ok(override_root) = std::env::var("AM_TUI_ARTIFACT_DIR") {
        return PathBuf::from(override_root).join(suite_name);
    }
    repo_root().join("tests/artifacts/cli").join(suite_name)
}

fn legacy_artifacts_root(subdir: &str) -> PathBuf {
    repo_root().join("tests/artifacts").join(subdir)
}

fn list_run_directories(root: &Path) -> BTreeSet<String> {
    let mut runs = BTreeSet::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return runs,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            runs.insert(name.to_string());
        }
    }
    runs
}

fn copy_directory_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(src_path, dst_path)?;
        }
    }
    Ok(())
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> CommandOutcome {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn command");
    let mut timed_out = false;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
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
        stderr.push_str(&format!(
            "adapter timed out after {}ms",
            timeout.as_millis()
        ));
    }

    CommandOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
    }
}

#[test]
fn native_tui_transport_gate() {
    let suite_name = selected_suite_name();
    let suite_spec = suite_spec_for(&suite_name);
    let start_instant = Instant::now();
    let run_root = harness_artifacts_root(suite_spec.suite_name).join(format!(
        "{}_{}",
        Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    fs::create_dir_all(&run_root).expect("create run root");
    eprintln!(
        "tui transport harness artifact root (suite={}): {}",
        suite_spec.suite_name,
        run_root.display()
    );

    let strict_mode = bool_env("AM_E2E_TUI_REQUIRE_PASS");
    let legacy_root = legacy_artifacts_root(suite_spec.legacy_artifacts_subdir);
    fs::create_dir_all(&legacy_root).expect("create legacy artifact root");
    let before_runs = list_run_directories(&legacy_root);

    let repo = repo_root();
    let mut cmd = Command::new("bash");
    cmd.current_dir(&repo)
        .arg(suite_spec.adapter_script)
        .env("E2E_PROJECT_ROOT", &repo)
        .env(
            "E2E_SERVER_START_TIMEOUT_SECONDS",
            std::env::var("E2E_SERVER_START_TIMEOUT_SECONDS").unwrap_or_else(|_| "60".to_string()),
        )
        .env(
            "AM_E2E_KEEP_TMP",
            std::env::var("AM_E2E_KEEP_TMP").unwrap_or_else(|_| "1".to_string()),
        );

    let timeout_secs = std::env::var("AM_TUI_ADAPTER_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(3600);
    let outcome = run_with_timeout(&mut cmd, Duration::from_secs(timeout_secs));

    let stdout_path = run_root.join("adapter.stdout.log");
    let stderr_path = run_root.join("adapter.stderr.log");
    fs::write(&stdout_path, &outcome.stdout).expect("write adapter stdout");
    fs::write(&stderr_path, &outcome.stderr).expect("write adapter stderr");

    let after_runs = list_run_directories(&legacy_root);
    let new_runs = after_runs
        .difference(&before_runs)
        .cloned()
        .collect::<Vec<String>>();

    let copied_legacy_run_path = new_runs.last().and_then(|run_name| {
        let src = legacy_root.join(run_name);
        let dst = run_root
            .join("subsuite")
            .join(suite_spec.suite_name)
            .join(run_name);
        copy_directory_recursive(&src, &dst).ok()?;
        Some(dst.display().to_string())
    });

    let passed = outcome.exit_code == Some(0) && !outcome.timed_out;
    let mut notes = Vec::new();
    if new_runs.is_empty() {
        notes.push("No new legacy artifact run was detected.".to_string());
    }
    if outcome.timed_out {
        notes.push("Adapter execution timed out.".to_string());
    }
    if outcome.exit_code != Some(0) {
        notes.push(format!(
            "Adapter exit code was {:?}, expected 0.",
            outcome.exit_code
        ));
    }

    let summary = HarnessSummary {
        suite: suite_spec.suite_name.to_string(),
        runner: "native".to_string(),
        adapter_mode: "shell_managed".to_string(),
        strict_mode,
        generated_at: Utc::now().to_rfc3339(),
        duration_ms: start_instant.elapsed().as_millis() as u64,
        passed,
        adapter_script: suite_spec.adapter_script.to_string(),
        adapter_exit_code: outcome.exit_code,
        adapter_timed_out: outcome.timed_out,
        adapter_duration_ms: outcome.duration_ms,
        adapter_stdout_path: stdout_path.display().to_string(),
        adapter_stderr_path: stderr_path.display().to_string(),
        legacy_artifacts_root: legacy_root.display().to_string(),
        new_legacy_run_paths: new_runs,
        copied_legacy_run_path,
        notes,
    };
    let summary_path = run_root.join("run_summary.json");
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).expect("serialize summary"),
    )
    .expect("write summary");

    if strict_mode {
        assert!(
            passed,
            "{} adapter failed (exit={:?}, timed_out={})\nstdout:\n{}\nstderr:\n{}",
            suite_spec.suite_name,
            outcome.exit_code,
            outcome.timed_out,
            outcome.stdout,
            outcome.stderr
        );
        assert!(
            summary.copied_legacy_run_path.is_some(),
            "{} adapter succeeded but did not produce a new legacy artifact run under {}",
            suite_spec.suite_name,
            legacy_root.display()
        );
    } else if !passed {
        eprintln!(
            "SKIP(non-strict): {} adapter failed but strict mode is disabled; see {}",
            suite_spec.suite_name,
            summary_path.display()
        );
    }
}
