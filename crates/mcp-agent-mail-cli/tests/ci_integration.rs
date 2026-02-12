//! Integration tests for `am ci` command (br-1dxs).
//!
//! Tests CLI flag parsing, report schema, and quick mode behavior.
//!
//! Note: Tests that would run the full CI suite are marked #[ignore] by default
//! to keep the test suite fast. Run with --include-ignored to execute them.

use std::path::PathBuf;
use std::process::Command;

/// Path to the compiled `am` binary.
fn am_binary() -> PathBuf {
    let cargo_bin = std::env::var("CARGO_BIN_EXE_am").ok();
    cargo_bin.map(PathBuf::from).unwrap_or_else(|| {
        let target_dir = std::env::var("CARGO_TARGET_DIR")
            .unwrap_or_else(|_| "/data/tmp/cargo-target".to_string());
        PathBuf::from(target_dir).join("debug/am")
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI Flag Parsing Tests (Fast - only checks help output)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_ci_help_shows_all_flags() {
    let output = Command::new(am_binary())
        .args(["ci", "--help"])
        .output()
        .expect("failed to execute am ci --help");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check all flags are documented
    assert!(stdout.contains("--quick"), "help should mention --quick");
    assert!(stdout.contains("--report"), "help should mention --report");
    assert!(stdout.contains("--json"), "help should mention --json");
    assert!(
        stdout.contains("--parallel"),
        "help should mention --parallel"
    );
    assert!(stdout.contains("-q"), "help should show -q shorthand");
    assert!(stdout.contains("-r"), "help should show -r shorthand");
    assert!(stdout.contains("-p"), "help should show -p shorthand");
}

#[test]
fn test_ci_help_exit_code_zero() {
    let output = Command::new(am_binary())
        .args(["ci", "--help"])
        .output()
        .expect("failed to execute am ci --help");

    assert!(
        output.status.success(),
        "am ci --help should exit with code 0"
    );
}

#[test]
fn test_ci_invalid_flag_rejected() {
    let output = Command::new(am_binary())
        .args(["ci", "--nonexistent-flag-xyz"])
        .output()
        .expect("failed to execute am ci");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "invalid flag should cause non-zero exit"
    );
    assert!(
        stderr.contains("error")
            || stderr.contains("unrecognized")
            || stderr.contains("unexpected"),
        "stderr should indicate error for invalid flag"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Report Schema Unit Tests (Fast - uses synthetic data)
// ─────────────────────────────────────────────────────────────────────────────

/// Test report schema validation using synthetic data (no actual CI execution).
#[test]
fn test_report_schema_has_required_fields() {
    use mcp_agent_mail_cli::ci::{GateCategory, GateReport, GateResult, GateStatus, RunMode};

    // Create a synthetic report
    let results = vec![
        GateResult {
            name: "Format check".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 2,
            command: "cargo fmt".to_string(),
            stderr_tail: None,
            error: None,
        },
        GateResult {
            name: "Build".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Fail,
            elapsed_seconds: 30,
            command: "cargo build".to_string(),
            stderr_tail: Some("error[E0425]: cannot find value".to_string()),
            error: Some(mcp_agent_mail_cli::ci::GateError::from_stderr(
                "error[E0425]: cannot find value",
            )),
        },
    ];

    let report = GateReport::new(RunMode::Full, results);
    let json = report.to_json().expect("serialization should work");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse JSON");

    // Check required top-level fields
    assert!(
        parsed["schema_version"].is_string(),
        "schema_version required"
    );
    assert!(parsed["generated_at"].is_string(), "generated_at required");
    assert!(parsed["mode"].is_string(), "mode required");
    assert!(parsed["decision"].is_string(), "decision required");
    assert!(
        parsed["decision_reason"].is_string(),
        "decision_reason required"
    );
    assert!(
        parsed["release_eligible"].is_boolean(),
        "release_eligible required"
    );
    assert!(
        parsed["total_elapsed_seconds"].is_number(),
        "total_elapsed_seconds required"
    );
    assert!(parsed["summary"].is_object(), "summary required");
    assert!(parsed["gates"].is_array(), "gates required");

    // Check summary sub-fields
    let summary = &parsed["summary"];
    assert!(summary["total"].is_number(), "summary.total required");
    assert!(summary["pass"].is_number(), "summary.pass required");
    assert!(summary["fail"].is_number(), "summary.fail required");
    assert!(summary["skip"].is_number(), "summary.skip required");

    // Check gate fields
    let gates = parsed["gates"].as_array().expect("gates is array");
    for gate in gates {
        assert!(gate["name"].is_string(), "gate.name required");
        assert!(gate["category"].is_string(), "gate.category required");
        assert!(gate["status"].is_string(), "gate.status required");
        assert!(
            gate["elapsed_seconds"].is_number(),
            "gate.elapsed_seconds required"
        );
        assert!(gate["command"].is_string(), "gate.command required");
    }
}

#[test]
fn test_report_decision_logic_all_pass_full_mode() {
    use mcp_agent_mail_cli::ci::{GateCategory, GateReport, GateResult, GateStatus, RunMode};

    let results = vec![GateResult {
        name: "Test".to_string(),
        category: GateCategory::Quality,
        status: GateStatus::Pass,
        elapsed_seconds: 10,
        command: "test".to_string(),
        stderr_tail: None,
        error: None,
    }];

    let report = GateReport::new(RunMode::Full, results);

    assert_eq!(report.decision.as_str(), "go");
    assert!(report.release_eligible);
}

#[test]
fn test_report_decision_logic_all_pass_quick_mode() {
    use mcp_agent_mail_cli::ci::{GateCategory, GateReport, GateResult, GateStatus, RunMode};

    let results = vec![GateResult {
        name: "Test".to_string(),
        category: GateCategory::Quality,
        status: GateStatus::Pass,
        elapsed_seconds: 10,
        command: "test".to_string(),
        stderr_tail: None,
        error: None,
    }];

    let report = GateReport::new(RunMode::Quick, results);

    // Quick mode = no-go even if all pass
    assert_eq!(report.decision.as_str(), "no-go");
    assert!(!report.release_eligible);
}

#[test]
fn test_report_decision_logic_one_failure() {
    use mcp_agent_mail_cli::ci::{GateCategory, GateReport, GateResult, GateStatus, RunMode};

    let results = vec![
        GateResult {
            name: "Pass".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 10,
            command: "test".to_string(),
            stderr_tail: None,
            error: None,
        },
        GateResult {
            name: "Fail".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Fail,
            elapsed_seconds: 5,
            command: "test".to_string(),
            stderr_tail: Some("error".to_string()),
            error: None,
        },
    ];

    let report = GateReport::new(RunMode::Full, results);

    assert_eq!(report.decision.as_str(), "no-go");
    assert!(!report.release_eligible);
    assert!(report.decision_reason.contains("failed"));
}

#[test]
fn test_default_gates_count() {
    use mcp_agent_mail_cli::ci::default_gates;

    let gates = default_gates();
    assert_eq!(gates.len(), 13, "should have 13 default gates");
}

#[test]
fn test_default_gates_skip_in_quick() {
    use mcp_agent_mail_cli::ci::default_gates;

    let gates = default_gates();
    let quick_skip: Vec<_> = gates.iter().filter(|g| g.skip_in_quick).collect();

    assert_eq!(quick_skip.len(), 4, "4 gates should skip in quick mode");

    let names: Vec<_> = quick_skip.iter().map(|g| g.name.as_str()).collect();
    assert!(names.contains(&"E2E dual-mode"));
    assert!(names.contains(&"E2E mode matrix"));
    assert!(names.contains(&"E2E security/privacy"));
    assert!(names.contains(&"E2E TUI accessibility"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Full CI Execution Tests (Slow - marked #[ignore])
// Run with: cargo test --test ci_integration -- --include-ignored
// ─────────────────────────────────────────────────────────────────────────────

/// Helper to run `am ci` with given args and return (stdout, stderr, exit_code).
#[cfg(test)]
fn run_am_ci(args: &[&str]) -> (String, String, i32) {
    let output = Command::new(am_binary())
        .arg("ci")
        .args(args)
        .output()
        .expect("failed to execute am ci");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);

    (stdout, stderr, code)
}

#[test]
#[ignore = "slow: runs full CI suite"]
fn test_ci_creates_report_file() {
    use std::fs;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let report_path = temp_dir.path().join("gate_report.json");

    let (_, _, code) = run_am_ci(&["--quick", "--report", report_path.to_str().unwrap()]);

    assert!(code == 0 || code == 1, "exit code should be 0 or 1");
    assert!(report_path.exists(), "report file should exist");

    let content = fs::read_to_string(&report_path).expect("read report");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse JSON");

    assert_eq!(parsed["schema_version"], "am_ci_gate_report.v1");
}

#[test]
#[ignore = "slow: runs full CI suite"]
fn test_ci_quick_mode_skips_e2e_gates() {
    use std::fs;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let report_path = temp_dir.path().join("gate_report.json");

    run_am_ci(&["--quick", "--report", report_path.to_str().unwrap()]);

    let content = fs::read_to_string(&report_path).expect("read report");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse JSON");

    let gates = parsed["gates"].as_array().expect("gates is array");
    let e2e_names = [
        "E2E dual-mode",
        "E2E mode matrix",
        "E2E security/privacy",
        "E2E TUI accessibility",
    ];

    for gate in gates {
        let name = gate["name"].as_str().unwrap_or("");
        if e2e_names.contains(&name) {
            assert_eq!(
                gate["status"], "skip",
                "E2E gate '{}' should be skipped",
                name
            );
        }
    }
}

#[test]
#[ignore = "slow: runs full CI suite"]
fn test_ci_parallel_produces_valid_report() {
    use std::fs;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let report_path = temp_dir.path().join("gate_report.json");

    run_am_ci(&[
        "--quick",
        "--parallel",
        "--report",
        report_path.to_str().unwrap(),
    ]);

    assert!(report_path.exists(), "report file should exist");

    let content = fs::read_to_string(&report_path).expect("read report");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse JSON");

    assert_eq!(parsed["schema_version"], "am_ci_gate_report.v1");
    assert_eq!(parsed["gates"].as_array().unwrap().len(), 13);
}
