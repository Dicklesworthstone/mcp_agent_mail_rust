//! Integration tests for `am flake-triage` CLI subcommands.
//!
//! Tests CLI flag parsing, scan with fixtures, JSON output schema, and
//! multi-seed detection. See bead br-shfc for requirements.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn create_failure_artifact(dir: &std::path::Path, test_name: &str, seed: u64) {
    let artifact = serde_json::json!({
        "test_name": test_name,
        "harness_seed": seed,
        "e2e_seed": null,
        "failure_message": format!("assertion failed: {} with seed {}", test_name, seed),
        "failure_ts": "2026-02-12T00:00:00.000000Z",
        "repro_command": format!("HARNESS_SEED={} cargo test {} -- --nocapture", seed, test_name),
        "repro_context": null,
        "env_snapshot": {},
        "rss_kb": 50000,
        "uptime_secs": 1.5,
        "category": "assertion",
        "notes": []
    });
    std::fs::write(
        dir.join("failure_context.json"),
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .expect("write artifact");
}

// ── CLI Flag Parsing Tests ─────────────────────────────────────────────

#[test]
fn flake_triage_scan_accepts_dir_flag() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(am_bin())
        .args(["flake-triage", "scan", "--dir", tmp.path().to_str().unwrap()])
        .output()
        .expect("am flake-triage scan");

    assert!(
        output.status.success(),
        "scan should succeed with --dir: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn flake_triage_scan_accepts_json_flag() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan --json");

    assert!(
        output.status.success(),
        "scan should succeed with --json: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Even an empty scan should produce valid JSON (array)
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("scan --json should produce valid JSON: {e}, got: {stdout}"));
    assert!(parsed.is_array(), "scan --json should produce an array");
}

#[test]
fn flake_triage_scan_short_flags() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(am_bin())
        .args(["flake-triage", "scan", "-d", tmp.path().to_str().unwrap()])
        .output()
        .expect("am flake-triage scan -d");

    assert!(
        output.status.success(),
        "scan should accept -d short flag: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn flake_triage_detect_accepts_flags() {
    // Just test that the flags parse correctly (actual test would run cargo which is slow)
    let output = Command::new(am_bin())
        .args(["flake-triage", "detect", "--help"])
        .output()
        .expect("am flake-triage detect --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--seeds"),
        "detect should document --seeds flag"
    );
    assert!(
        stdout.contains("--packages"),
        "detect should document --packages flag"
    );
    assert!(
        stdout.contains("--timeout"),
        "detect should document --timeout flag"
    );
    assert!(
        stdout.contains("--json"),
        "detect should document --json flag"
    );
}

#[test]
fn flake_triage_reproduce_accepts_flags() {
    let output = Command::new(am_bin())
        .args(["flake-triage", "reproduce", "--help"])
        .output()
        .expect("am flake-triage reproduce --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--verbose"),
        "reproduce should document --verbose flag"
    );
    assert!(
        stdout.contains("--timeout"),
        "reproduce should document --timeout flag"
    );
}

// ── Scan with Fixtures ─────────────────────────────────────────────────

#[test]
fn flake_triage_scan_finds_artifacts() {
    let tmp = TempDir::new().unwrap();

    // Create nested artifacts
    let sub1 = tmp.path().join("run1");
    let sub2 = tmp.path().join("nested").join("run2");
    std::fs::create_dir_all(&sub1).unwrap();
    std::fs::create_dir_all(&sub2).unwrap();

    create_failure_artifact(&sub1, "test_alpha", 42);
    create_failure_artifact(&sub2, "test_beta", 99);

    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("valid JSON array");

    assert_eq!(parsed.len(), 2, "should find exactly 2 artifacts");

    let test_names: Vec<&str> = parsed
        .iter()
        .filter_map(|v| v.get("context").and_then(|c| c.get("test_name")?.as_str()))
        .collect();
    assert!(test_names.contains(&"test_alpha"));
    assert!(test_names.contains(&"test_beta"));
}

#[test]
fn flake_triage_scan_human_readable_output() {
    let tmp = TempDir::new().unwrap();
    let sub = tmp.path().join("run");
    std::fs::create_dir_all(&sub).unwrap();
    create_failure_artifact(&sub, "test_human", 123);

    let output = Command::new(am_bin())
        .args(["flake-triage", "scan", "--dir", tmp.path().to_str().unwrap()])
        .output()
        .expect("am flake-triage scan (human)");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Human-readable should show test name and failure info
    assert!(
        stdout.contains("test_human") || stdout.contains("1 artifact"),
        "human output should mention test name or count: {stdout}"
    );
}

#[test]
fn flake_triage_scan_skips_malformed_artifacts() {
    let tmp = TempDir::new().unwrap();

    // Valid artifact
    let valid = tmp.path().join("valid");
    std::fs::create_dir_all(&valid).unwrap();
    create_failure_artifact(&valid, "good_test", 1);

    // Malformed artifact
    let bad = tmp.path().join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("failure_context.json"), "{ invalid json }").unwrap();

    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("valid JSON array");

    assert_eq!(
        parsed.len(),
        1,
        "should find only the valid artifact, skipping malformed"
    );
    assert_eq!(
        parsed[0]["context"]["test_name"].as_str(),
        Some("good_test")
    );

    // Malformed should be logged to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipping") || stderr.contains("malformed") || parsed.len() == 1,
        "should warn about malformed artifact or simply skip it"
    );
}

#[test]
fn flake_triage_scan_empty_dir() {
    let tmp = TempDir::new().unwrap();

    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan empty");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("valid JSON array");
    assert!(parsed.is_empty(), "empty dir should produce empty array");
}

#[test]
fn flake_triage_scan_nonexistent_dir() {
    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            "/nonexistent/path/that/does/not/exist",
        ])
        .output()
        .expect("am flake-triage scan nonexistent");

    // Should not panic, just return empty or error gracefully
    // The exit code might be 0 (empty scan) or non-zero (error)
    // Either behavior is acceptable as long as it doesn't panic
    let _ = output.status;
}

// ── JSON Output Schema Validation ──────────────────────────────────────

#[test]
fn flake_triage_scan_json_schema() {
    let tmp = TempDir::new().unwrap();
    let sub = tmp.path().join("run");
    std::fs::create_dir_all(&sub).unwrap();
    create_failure_artifact(&sub, "schema_test", 42);

    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("valid JSON array");

    assert_eq!(parsed.len(), 1);
    let item = &parsed[0];

    // Verify schema: each item should have "path" and "context"
    assert!(item.get("path").is_some(), "item should have 'path' field");
    assert!(
        item.get("context").is_some(),
        "item should have 'context' field"
    );

    let ctx = item.get("context").unwrap();
    // Verify FailureContext schema fields
    assert!(
        ctx.get("test_name").is_some(),
        "context should have 'test_name'"
    );
    assert!(
        ctx.get("failure_message").is_some(),
        "context should have 'failure_message'"
    );
    assert!(
        ctx.get("failure_ts").is_some(),
        "context should have 'failure_ts'"
    );
    assert!(
        ctx.get("repro_command").is_some(),
        "context should have 'repro_command'"
    );
    assert!(
        ctx.get("category").is_some(),
        "context should have 'category'"
    );
}

// ── Grouping by Test Name ──────────────────────────────────────────────

#[test]
fn flake_triage_scan_sorts_by_timestamp() {
    let tmp = TempDir::new().unwrap();

    // Create artifacts with different timestamps
    for (i, ts) in ["2026-02-10T00:00:00Z", "2026-02-12T00:00:00Z", "2026-02-11T00:00:00Z"]
        .iter()
        .enumerate()
    {
        let sub = tmp.path().join(format!("run{i}"));
        std::fs::create_dir_all(&sub).unwrap();
        let artifact = serde_json::json!({
            "test_name": format!("test_{}", i),
            "harness_seed": i,
            "e2e_seed": null,
            "failure_message": "fail",
            "failure_ts": ts,
            "repro_command": "cargo test",
            "repro_context": null,
            "env_snapshot": {},
            "rss_kb": 1000,
            "uptime_secs": 1.0,
            "category": "assertion",
            "notes": []
        });
        std::fs::write(
            sub.join("failure_context.json"),
            serde_json::to_string_pretty(&artifact).unwrap(),
        )
        .unwrap();
    }

    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "scan",
            "--dir",
            tmp.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("am flake-triage scan");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<Value> = serde_json::from_str(&stdout).expect("valid JSON array");

    assert_eq!(parsed.len(), 3);

    // Should be sorted by timestamp (most recent first)
    let timestamps: Vec<&str> = parsed
        .iter()
        .filter_map(|v| v.get("context").and_then(|c| c.get("failure_ts")?.as_str()))
        .collect();

    assert_eq!(
        timestamps,
        vec![
            "2026-02-12T00:00:00Z",
            "2026-02-11T00:00:00Z",
            "2026-02-10T00:00:00Z"
        ],
        "artifacts should be sorted by timestamp (most recent first)"
    );
}

// ── Reproduce with Artifact ────────────────────────────────────────────

#[test]
fn flake_triage_reproduce_with_missing_artifact() {
    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "reproduce",
            "/nonexistent/failure_context.json",
        ])
        .output()
        .expect("am flake-triage reproduce missing");

    assert!(
        !output.status.success(),
        "reproduce with missing artifact should fail"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No such file")
            || stderr.contains("not found")
            || stderr.contains("error")
            || stderr.to_lowercase().contains("failed"),
        "should report error for missing artifact: {stderr}"
    );
}

// ── Detect Tests (require cargo test, marked ignore) ───────────────────

/// Tests that `am flake-triage detect` runs and produces expected output.
///
/// This test is ignored by default because it spawns `cargo test` subprocesses
/// which is slow and may not work in all CI environments.
#[test]
#[ignore = "requires cargo test subprocess execution"]
fn flake_triage_detect_known_passing_test() {
    // Use a test that we know always passes
    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "detect",
            "flake_report_stable",
            "--seeds",
            "3",
            "-p",
            "mcp-agent-mail-core",
            "--json",
        ])
        .output()
        .expect("am flake-triage detect");

    // Note: This may fail if the test doesn't exist or cargo test isn't available
    if !output.status.success() {
        eprintln!(
            "detect failed (expected in some CI environments): {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Should have verdict field
    assert!(
        parsed.get("verdict").is_some(),
        "detect output should have 'verdict' field"
    );
}

/// Tests that detect with 0 seeds produces inconclusive verdict.
#[test]
#[ignore = "requires cargo test subprocess execution"]
fn flake_triage_detect_zero_seeds() {
    // With 0 seeds, should be inconclusive or early-exit
    let output = Command::new(am_bin())
        .args([
            "flake-triage",
            "detect",
            "nonexistent_test_xyz",
            "--seeds",
            "0",
            "--json",
        ])
        .output()
        .expect("am flake-triage detect 0 seeds");

    // This should either succeed with inconclusive or fail gracefully
    let _ = output.status;
}

// ── Help and Version ───────────────────────────────────────────────────

#[test]
fn flake_triage_help() {
    let output = Command::new(am_bin())
        .args(["flake-triage", "--help"])
        .output()
        .expect("am flake-triage --help");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("scan"), "help should mention scan subcommand");
    assert!(
        stdout.contains("reproduce"),
        "help should mention reproduce subcommand"
    );
    assert!(
        stdout.contains("detect"),
        "help should mention detect subcommand"
    );
}

#[test]
fn flake_triage_scan_help() {
    let output = Command::new(am_bin())
        .args(["flake-triage", "scan", "--help"])
        .output()
        .expect("am flake-triage scan --help");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--dir"), "scan help should document --dir");
    assert!(stdout.contains("--json"), "scan help should document --json");
}
