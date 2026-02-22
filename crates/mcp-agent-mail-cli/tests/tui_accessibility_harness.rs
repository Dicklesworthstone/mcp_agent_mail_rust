//! br-1dsl: Native TUI accessibility migration harness.
//!
//! Hybrid strategy:
//! - Native deterministic portion: contrast threshold test.
//! - Managed adapter portion: shell/expect keyboard flows with machine-readable
//!   adapter metadata and artifact contract checks.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use mcp_agent_mail_server::tui_theme::{
    MIN_THEME_ACCENT_CONTRAST, MIN_THEME_TEXT_CONTRAST, collect_theme_contrast_metrics,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug)]
struct CommandOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    timed_out: bool,
}

#[derive(Debug, Serialize)]
struct CheckRecord {
    scenario_id: String,
    expected: String,
    actual: String,
    passed: bool,
    debug_hint: String,
}

#[derive(Debug, Serialize)]
struct StepRecord {
    step: usize,
    name: String,
    command: String,
    cwd: String,
    timeout_ms: u64,
    duration_ms: u64,
    exit_code: Option<i32>,
    timed_out: bool,
    passed: bool,
    stdout_path: String,
    stderr_path: String,
    checks: Vec<CheckRecord>,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    suite: String,
    runner: String,
    generated_at: String,
    duration_ms: u64,
    passed: bool,
    require_no_skip: bool,
    steps: Vec<StepRecord>,
    failures: Vec<CheckRecord>,
    environment: Value,
}

#[derive(Debug, Deserialize)]
struct AdapterResult {
    suite: String,
    timestamp: String,
    status: String,
    exit_code: i64,
    artifact_dir: String,
    summary_path: String,
    bundle_path: String,
    trace_path: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    if let Ok(override_root) = std::env::var("AM_TUI_A11Y_ARTIFACT_DIR") {
        return PathBuf::from(override_root).join("tui_a11y");
    }
    repo_root().join("tests/artifacts/cli/tui_a11y")
}

fn sanitize_for_filename(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> CommandOutcome {
    let stdout_temp = tempfile::NamedTempFile::new().expect("create stdout temp file");
    let stderr_temp = tempfile::NamedTempFile::new().expect("create stderr temp file");
    let stdout_writer = stdout_temp.reopen().expect("reopen stdout temp file");
    let stderr_writer = stderr_temp.reopen().expect("reopen stderr temp file");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::from(stderr_writer));

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

    let status = child.wait().expect("wait child");
    let stdout_bytes = fs::read(stdout_temp.path()).unwrap_or_default();
    let stderr_bytes = fs::read(stderr_temp.path()).unwrap_or_default();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "command timed out after {}ms",
            timeout.as_millis()
        ));
    }

    CommandOutcome {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
    }
}

fn write_step_outputs(
    run_root: &Path,
    step: usize,
    name: &str,
    outcome: &CommandOutcome,
) -> (PathBuf, PathBuf) {
    let slug = sanitize_for_filename(name);
    let out_path = run_root
        .join("steps")
        .join(format!("step_{step:03}_{slug}.stdout.log"));
    let err_path = run_root
        .join("steps")
        .join(format!("step_{step:03}_{slug}.stderr.log"));
    fs::write(&out_path, &outcome.stdout).expect("write step stdout");
    fs::write(&err_path, &outcome.stderr).expect("write step stderr");
    (out_path, err_path)
}

fn push_check(
    checks: &mut Vec<CheckRecord>,
    failures: &mut Vec<CheckRecord>,
    scenario_id: &str,
    expected: impl Into<String>,
    actual: impl Into<String>,
    passed: bool,
    debug_hint: impl Into<String>,
) {
    let check = CheckRecord {
        scenario_id: scenario_id.to_string(),
        expected: expected.into(),
        actual: actual.into(),
        passed,
        debug_hint: debug_hint.into(),
    };
    if !check.passed {
        failures.push(CheckRecord {
            scenario_id: check.scenario_id.clone(),
            expected: check.expected.clone(),
            actual: check.actual.clone(),
            passed: false,
            debug_hint: check.debug_hint.clone(),
        });
    }
    checks.push(check);
}

fn as_u64(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

#[test]
fn native_tui_accessibility_gate() {
    let run_start = Instant::now();
    let run_root = artifacts_dir().join(format!(
        "{}_{}",
        Utc::now().format("%Y%m%d_%H%M%S%.3fZ"),
        std::process::id()
    ));
    fs::create_dir_all(run_root.join("steps")).expect("create steps dir");
    fs::create_dir_all(run_root.join("failures")).expect("create failures dir");
    eprintln!("tui_a11y artifact root: {}", run_root.display());

    let require_no_skip = std::env::var("AM_E2E_TUI_A11Y_REQUIRE_NO_SKIP")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));

    let mut steps = Vec::new();
    let mut failures = Vec::new();
    let repo_root = repo_root();

    // Step 1: native contrast threshold gate.
    let step = 1usize;
    let mut checks = Vec::new();
    let step_started = Instant::now();
    let metrics = collect_theme_contrast_metrics();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut failures_by_theme = Vec::new();
    for metric in &metrics {
        stdout.push_str(&metric.log_line());
        stdout.push('\n');
        for (dimension, value, minimum) in
            metric.failing_dimensions(MIN_THEME_TEXT_CONTRAST, MIN_THEME_ACCENT_CONTRAST)
        {
            failures_by_theme.push((metric.theme, dimension, value, minimum));
        }
    }
    for (theme, dimension, value, minimum) in &failures_by_theme {
        stderr.push_str(&format!(
            "theme={theme:?} {dimension}={value:.2} minimum={minimum:.1}\n"
        ));
    }
    let outcome = CommandOutcome {
        exit_code: Some(if failures_by_theme.is_empty() { 0 } else { 1 }),
        stdout,
        stderr,
        duration_ms: step_started.elapsed().as_millis() as u64,
        timed_out: false,
    };
    let (stdout_path, stderr_path) =
        write_step_outputs(&run_root, step, "contrast_native", &outcome);
    let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);
    push_check(
        &mut checks,
        &mut failures,
        "contrast.exit_code",
        "exit_code == 0",
        format!("exit_code={:?}", outcome.exit_code),
        outcome.exit_code == Some(0) && !outcome.timed_out,
        "Inspect step_001 logs for dimension-level threshold failures.",
    );
    push_check(
        &mut checks,
        &mut failures,
        "contrast.output_contains_theme",
        "output contains `theme=` lines",
        if combined.contains("theme=") {
            "contains theme=".to_string()
        } else {
            "missing theme=".to_string()
        },
        combined.contains("theme="),
        "Ensure collect_theme_contrast_metrics() emits one line per theme.",
    );
    let step_passed = checks.iter().all(|check| check.passed);
    steps.push(StepRecord {
        step,
        name: "contrast_native".to_string(),
        command: "native tui_theme::collect_theme_contrast_metrics() threshold gate".to_string(),
        cwd: repo_root.display().to_string(),
        timeout_ms: 5_000,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        passed: step_passed,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        checks,
    });

    // Step 2: shell adapter run with structured metadata contract.
    let step = 2usize;
    let mut checks = Vec::new();
    let adapter_manifest = run_root.join("adapter_result.json");
    let adapter_target_dir = run_root.join("cargo_target");
    fs::create_dir_all(&adapter_target_dir).expect("create adapter target dir");
    let seed = 2_026_021_300u64 + u64::from(std::process::id());
    let mut cmd = Command::new("bash");
    cmd.current_dir(&repo_root)
        .arg("scripts/e2e_tui_a11y.sh")
        .env("AM_E2E_KEEP_TMP", "1")
        .env("CARGO_TARGET_DIR", adapter_target_dir.display().to_string())
        .env("E2E_CLOCK_MODE", "deterministic")
        .env("E2E_SEED", seed.to_string())
        .env("AM_TUI_A11Y_SKIP_CONTRAST", "1")
        .env(
            "AM_TUI_A11Y_ADAPTER_OUTPUT",
            adapter_manifest.display().to_string(),
        );
    let outcome = run_with_timeout(&mut cmd, Duration::from_secs(900));
    let (stdout_path, stderr_path) = write_step_outputs(&run_root, step, "shell_adapter", &outcome);
    push_check(
        &mut checks,
        &mut failures,
        "adapter.exit_code",
        "adapter exit_code == 0",
        format!("exit_code={:?}", outcome.exit_code),
        outcome.exit_code == Some(0) && !outcome.timed_out,
        "Open step_002 logs to identify failing case in scripts/e2e_tui_a11y.sh.",
    );
    push_check(
        &mut checks,
        &mut failures,
        "adapter.manifest_exists",
        "adapter_result.json exists",
        adapter_manifest.display().to_string(),
        adapter_manifest.exists(),
        "Verify AM_TUI_A11Y_ADAPTER_OUTPUT is set and writable.",
    );

    let mut adapter_summary_path = String::new();
    let mut adapter_bundle_path = String::new();
    let mut adapter_trace_path = String::new();
    let mut adapter_artifact_dir = String::new();
    let mut adapter_status = String::new();
    let mut adapter_timestamp = String::new();
    let mut adapter_suite = String::new();
    let mut adapter_exit_code = -1i64;

    if adapter_manifest.exists() {
        let manifest_text = fs::read_to_string(&adapter_manifest).expect("read adapter manifest");
        let parsed: AdapterResult =
            serde_json::from_str(&manifest_text).expect("parse adapter manifest");
        adapter_summary_path = parsed.summary_path;
        adapter_bundle_path = parsed.bundle_path;
        adapter_trace_path = parsed.trace_path;
        adapter_artifact_dir = parsed.artifact_dir;
        adapter_status = parsed.status;
        adapter_timestamp = parsed.timestamp;
        adapter_suite = parsed.suite;
        adapter_exit_code = parsed.exit_code;

        push_check(
            &mut checks,
            &mut failures,
            "adapter.suite_name",
            "adapter suite == tui_a11y",
            format!("suite={adapter_suite}"),
            adapter_suite == "tui_a11y",
            "Ensure scripts/e2e_tui_a11y.sh writes suite metadata from E2E_SUITE.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.status",
            "adapter status == pass",
            format!("status={adapter_status}"),
            adapter_status == "pass",
            "Inspect summary.json + trace/events.jsonl in adapter artifact dir.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.exit_code_field",
            "adapter exit_code field == 0",
            format!("exit_code={adapter_exit_code}"),
            adapter_exit_code == 0,
            "Adapter metadata must mirror the actual script exit code.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.summary_exists",
            "summary_path points to existing file",
            adapter_summary_path.clone(),
            Path::new(&adapter_summary_path).exists(),
            "summary.json should always be emitted by e2e_summary.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.bundle_exists",
            "bundle_path points to existing file",
            adapter_bundle_path.clone(),
            Path::new(&adapter_bundle_path).exists(),
            "bundle.json is required for forensic artifact contract.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.trace_exists",
            "trace_path points to existing file",
            adapter_trace_path.clone(),
            Path::new(&adapter_trace_path).exists(),
            "trace/events.jsonl is required for step-by-step command traceability.",
        );
    }

    if !adapter_summary_path.is_empty() && Path::new(&adapter_summary_path).exists() {
        let summary_text = fs::read_to_string(&adapter_summary_path).expect("read summary");
        let summary: Value = serde_json::from_str(&summary_text).expect("parse summary");
        let total = as_u64(&summary, "total");
        let pass = as_u64(&summary, "pass");
        let fail = as_u64(&summary, "fail");
        let skip = as_u64(&summary, "skip");

        push_check(
            &mut checks,
            &mut failures,
            "adapter.summary_fail_zero",
            "summary.fail == 0",
            format!("fail={fail}"),
            fail == 0,
            "Open trace/events.jsonl and case artifacts to find first failing assertion.",
        );
        push_check(
            &mut checks,
            &mut failures,
            "adapter.summary_pass_positive",
            "summary.pass >= 1",
            format!("pass={pass}"),
            pass >= 1,
            "A fully skipped suite should not satisfy migration evidence requirements.",
        );
        if require_no_skip {
            push_check(
                &mut checks,
                &mut failures,
                "adapter.summary_skip_zero",
                "summary.skip <= 1 (contrast delegation only)",
                format!("skip={skip}"),
                skip <= 1,
                "Only contrast delegation may be skipped; all interactive a11y cases must run.",
            );
            push_check(
                &mut checks,
                &mut failures,
                "adapter.summary_total_minimum",
                "summary.total >= 4 (all a11y cases executed)",
                format!("total={total}"),
                total >= 4,
                "Ensure keyboard and key-hint scenarios are not bypassed.",
            );
        }
    }

    if !adapter_artifact_dir.is_empty() {
        let required_rel = [
            "trace/core_focus_trace.jsonl",
            "core_screens.rendered.txt",
            "key_hints_default.rendered.txt",
            "key_hints.rendered.txt",
        ];
        for rel in required_rel {
            let path = Path::new(&adapter_artifact_dir).join(rel);
            push_check(
                &mut checks,
                &mut failures,
                &format!("adapter.artifact.{rel}"),
                "required adapter artifact exists",
                path.display().to_string(),
                path.exists(),
                "Confirm the expect flow completed and raw PTY logs were rendered.",
            );
        }
    }

    let step_passed = checks.iter().all(|check| check.passed);
    steps.push(StepRecord {
        step,
        name: "shell_adapter".to_string(),
        command: "bash scripts/e2e_tui_a11y.sh (CARGO_TARGET_DIR=<isolated>, AM_TUI_A11Y_SKIP_CONTRAST=1, AM_TUI_A11Y_ADAPTER_OUTPUT=<path>)".to_string(),
        cwd: repo_root.display().to_string(),
        timeout_ms: 900_000,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        passed: step_passed,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        checks,
    });

    let summary = RunSummary {
        suite: "tui_a11y".to_string(),
        runner: "native_hybrid".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        duration_ms: run_start.elapsed().as_millis() as u64,
        passed: failures.is_empty(),
        require_no_skip,
        steps,
        failures,
        environment: serde_json::json!({
            "repo_root": repo_root.display().to_string(),
            "artifact_root": run_root.display().to_string(),
            "strict_skip_enforcement": require_no_skip,
            "adapter": {
                "suite": adapter_suite,
                "timestamp": adapter_timestamp,
                "status": adapter_status,
                "exit_code": adapter_exit_code,
                "artifact_dir": adapter_artifact_dir,
                "cargo_target_dir": adapter_target_dir.display().to_string(),
                "summary_path": adapter_summary_path,
                "bundle_path": adapter_bundle_path,
                "trace_path": adapter_trace_path
            }
        }),
    };

    let run_summary_path = run_root.join("run_summary.json");
    fs::write(
        &run_summary_path,
        serde_json::to_string_pretty(&summary).expect("serialize run summary"),
    )
    .expect("write run summary");

    if !summary.passed {
        let failure_report = summary
            .failures
            .iter()
            .map(|failure| {
                format!(
                    "- {}: expected `{}` actual `{}` | next: {}",
                    failure.scenario_id, failure.expected, failure.actual, failure.debug_hint
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "native tui_a11y harness failed:\n{failure_report}\nrun summary: {}",
            run_summary_path.display()
        );
    }
}
