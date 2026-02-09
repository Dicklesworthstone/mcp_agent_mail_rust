//! br-21gj.5.4: Performance + security regressions on conformance-validated surface.
//!
//! Enforces SLO/security constraints on the dual-mode command surface.
//! Does NOT test semantic correctness (that's br-21gj.5.3) or routing
//! (that's br-21gj.5.2). Instead: dispatch latency budgets, mode-bypass
//! attempts, malformed mode inputs, and denial-guard evasion.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn mcp_bin() -> Option<PathBuf> {
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp = target_dir.join("mcp-agent-mail");
    mcp.exists().then_some(mcp)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    repo_root().join("tests/artifacts/cli/perf_security")
}

// ── Structured artifacts ─────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct PerfRow {
    binary: String,
    command: String,
    iterations: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    budget_p95_us: u64,
    passed: bool,
}

#[derive(Debug, serde::Serialize)]
struct SecurityRow {
    attack_class: String,
    description: String,
    input: String,
    expected_behavior: String,
    actual_exit_code: Option<i32>,
    actual_stderr_contains: Vec<String>,
    passed: bool,
}

fn save_artifact<T: serde::Serialize>(data: &T, name: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let dir = artifacts_dir().join(format!("{ts}_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(data).unwrap_or_default();
    let _ = std::fs::write(&path, &json);
    eprintln!("artifact: {}", path.display());
}

// ── Helpers ──────────────────────────────────────────────────────────

fn run_binary(bin: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn binary")
}

fn run_binary_with_env(bin: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn binary")
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Performance Tests ────────────────────────────────────────────────

/// PERF-1: MCP denial gate dispatch latency.
/// The denial path should complete in well under 100ms p95.
/// Budget: p95 < 50ms (50000µs). This is generous — we expect <10ms.
#[test]
fn perf_mcp_denial_gate_dispatch_latency() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let iterations = 20;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let _ = run_binary(&mcp, &["share"]);
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
    }

    latencies.sort();
    let budget_p95 = 50_000; // 50ms

    let row = PerfRow {
        binary: "mcp-agent-mail".to_string(),
        command: "share (denied)".to_string(),
        iterations,
        p50_us: percentile(&latencies, 50.0),
        p95_us: percentile(&latencies, 95.0),
        p99_us: percentile(&latencies, 99.0),
        max_us: *latencies.last().unwrap_or(&0),
        budget_p95_us: budget_p95,
        passed: percentile(&latencies, 95.0) < budget_p95,
    };
    save_artifact(&row, "perf_denial_gate");
    eprintln!(
        "denial gate: p50={:.1}ms p95={:.1}ms p99={:.1}ms",
        row.p50_us as f64 / 1000.0,
        row.p95_us as f64 / 1000.0,
        row.p99_us as f64 / 1000.0,
    );

    assert!(
        row.passed,
        "denial gate p95 {:.1}ms exceeds budget {:.1}ms",
        row.p95_us as f64 / 1000.0,
        budget_p95 as f64 / 1000.0,
    );
}

/// PERF-2: CLI --help dispatch latency.
/// Measures cold clap parse + render. Budget: p95 < 100ms.
#[test]
fn perf_cli_help_dispatch_latency() {
    let am = am_bin();

    let iterations = 20;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let _ = run_binary(&am, &["--help"]);
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
    }

    latencies.sort();
    let budget_p95 = 100_000; // 100ms

    let row = PerfRow {
        binary: "am".to_string(),
        command: "--help".to_string(),
        iterations,
        p50_us: percentile(&latencies, 50.0),
        p95_us: percentile(&latencies, 95.0),
        p99_us: percentile(&latencies, 99.0),
        max_us: *latencies.last().unwrap_or(&0),
        budget_p95_us: budget_p95,
        passed: percentile(&latencies, 95.0) < budget_p95,
    };
    save_artifact(&row, "perf_cli_help");
    eprintln!(
        "CLI --help: p50={:.1}ms p95={:.1}ms p99={:.1}ms",
        row.p50_us as f64 / 1000.0,
        row.p95_us as f64 / 1000.0,
        row.p99_us as f64 / 1000.0,
    );

    assert!(
        row.passed,
        "CLI --help p95 {:.1}ms exceeds budget {:.1}ms",
        row.p95_us as f64 / 1000.0,
        budget_p95 as f64 / 1000.0,
    );
}

/// PERF-3: MCP `config` (allowed path) dispatch latency.
/// Budget: p95 < 100ms.
#[test]
fn perf_mcp_config_dispatch_latency() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let iterations = 20;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let _ = run_binary_with_env(
            &mcp,
            &["config"],
            &[
                ("DATABASE_URL", "sqlite:///tmp/perf_test.db"),
                ("STORAGE_ROOT", "/tmp"),
                ("AGENT_NAME", "PerfTest"),
                ("HTTP_HOST", "127.0.0.1"),
                ("HTTP_PORT", "1"),
                ("HTTP_PATH", "/mcp/"),
            ],
        );
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
    }

    latencies.sort();
    let budget_p95 = 100_000; // 100ms

    let row = PerfRow {
        binary: "mcp-agent-mail".to_string(),
        command: "config".to_string(),
        iterations,
        p50_us: percentile(&latencies, 50.0),
        p95_us: percentile(&latencies, 95.0),
        p99_us: percentile(&latencies, 99.0),
        max_us: *latencies.last().unwrap_or(&0),
        budget_p95_us: budget_p95,
        passed: percentile(&latencies, 95.0) < budget_p95,
    };
    save_artifact(&row, "perf_mcp_config");
    eprintln!(
        "MCP config: p50={:.1}ms p95={:.1}ms p99={:.1}ms",
        row.p50_us as f64 / 1000.0,
        row.p95_us as f64 / 1000.0,
        row.p99_us as f64 / 1000.0,
    );

    assert!(
        row.passed,
        "MCP config p95 {:.1}ms exceeds budget {:.1}ms",
        row.p95_us as f64 / 1000.0,
        budget_p95 as f64 / 1000.0,
    );
}

// ── Security Tests ───────────────────────────────────────────────────

/// SEC-1: Denial gate cannot be bypassed with path traversal in command name.
/// Attempts like `../../share`, `./guard`, `../bin/share` must still be denied.
#[test]
fn sec_denial_gate_path_traversal() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let traversal_attempts = &[
        ("../../share", "parent dir traversal"),
        ("./guard", "current dir prefix"),
        ("..%2F..%2Fshare", "URL-encoded traversal"),
        ("share/../serve", "traversal to allowed command"),
        ("/share", "absolute path prefix"),
    ];

    let mut results = Vec::new();

    for (input, desc) in traversal_attempts {
        let out = run_binary(&mcp, &[input]);
        let exit = out.status.code();
        let serr = String::from_utf8_lossy(&out.stderr).to_string();

        // All of these must be denied (exit 2) or rejected by clap (exit 2).
        // They must NOT exit 0 (allowed through).
        let passed = exit != Some(0);

        results.push(SecurityRow {
            attack_class: "path_traversal".to_string(),
            description: desc.to_string(),
            input: input.to_string(),
            expected_behavior: "denied (exit != 0)".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: if serr.is_empty() {
                vec![]
            } else {
                vec![serr.lines().next().unwrap_or("").to_string()]
            },
            passed,
        });
    }

    save_artifact(&results, "sec_path_traversal");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "path traversal bypass attempts: {} of {} not blocked:\n{}",
        failures.len(),
        results.len(),
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.input, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-2: Denial gate handles excessively long command names without panicking.
/// Should exit cleanly with denial (exit 2), not crash or hang.
#[test]
fn sec_denial_gate_oversized_command() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let huge_cmd = "A".repeat(10_000);
    let out = run_binary(&mcp, &[&huge_cmd]);
    let exit = out.status.code();

    let passed = exit == Some(2);
    let serr = String::from_utf8_lossy(&out.stderr).to_string();

    let row = SecurityRow {
        attack_class: "oversized_input".to_string(),
        description: "10KB command name".to_string(),
        input: "(10000 × 'A')".to_string(),
        expected_behavior: "denied (exit 2)".to_string(),
        actual_exit_code: exit,
        actual_stderr_contains: if serr.contains("not an MCP server command") {
            vec!["not an MCP server command".to_string()]
        } else {
            vec![serr.lines().next().unwrap_or("").to_string()]
        },
        passed,
    };
    save_artifact(&row, "sec_oversized_command");

    assert!(
        passed,
        "oversized command should be denied (exit 2), got exit {:?}",
        exit
    );
}

/// SEC-3: Denial gate handles unicode/special chars in command names.
/// Ensures no crash or bypass via non-ASCII or control characters.
#[test]
fn sec_denial_gate_unicode_and_control_chars() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // Note: null bytes (\x00) cannot be passed as process args (OS rejects them),
    // so we test only non-null special characters here.
    let special_inputs = &[
        ("ŝěŗvé", "unicode homoglyphs"),
        ("🚀share", "emoji prefix"),
        ("serve\t--help", "tab injection"),
        ("share\nserve", "newline injection"),
        (
            "serve\u{200B}guard",
            "zero-width space between serve and guard",
        ),
        ("‒help", "en-dash instead of hyphen"),
    ];

    let mut results = Vec::new();

    for (input, desc) in special_inputs {
        let out = match Command::new(&mcp)
            .args([*input])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) => o,
            Err(_) => {
                // OS rejected the input (e.g., embedded newlines on some OSes).
                // That counts as "not allowed through" — pass.
                results.push(SecurityRow {
                    attack_class: "special_chars".to_string(),
                    description: desc.to_string(),
                    input: format!("{:?}", input),
                    expected_behavior: "denied or rejected (exit != 0)".to_string(),
                    actual_exit_code: None,
                    actual_stderr_contains: vec!["OS rejected".to_string()],
                    passed: true,
                });
                continue;
            }
        };
        let exit = out.status.code();

        // None of these should exit 0 (allowed through serve/config).
        let passed = exit != Some(0);

        results.push(SecurityRow {
            attack_class: "special_chars".to_string(),
            description: desc.to_string(),
            input: format!("{:?}", input),
            expected_behavior: "denied or rejected (exit != 0)".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: vec![],
            passed,
        });
    }

    save_artifact(&results, "sec_special_chars");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "special char bypass attempts: {} not blocked:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.description, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-4: Env var INTERFACE_MODE override cannot bypass MCP denial gate.
/// Even if someone sets INTERFACE_MODE=agent, the MCP binary must still deny.
#[test]
fn sec_env_mode_override_cannot_bypass_denial() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let env_overrides = &[
        ("INTERFACE_MODE", "agent"),
        ("INTERFACE_MODE", "cli"),
        ("INTERFACE_MODE", "all"),
        ("MCP_MODE", "agent"),
    ];

    let test_command = "share";
    let mut results = Vec::new();

    for (env_key, env_val) in env_overrides {
        let out = run_binary_with_env(&mcp, &[test_command], &[(env_key, env_val)]);
        let exit = out.status.code();

        // Must still deny (exit 2) regardless of env override.
        let passed = exit == Some(2);

        results.push(SecurityRow {
            attack_class: "env_mode_override".to_string(),
            description: format!("{env_key}={env_val}"),
            input: format!("{env_key}={env_val} mcp-agent-mail {test_command}"),
            expected_behavior: "denied (exit 2)".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: vec![],
            passed,
        });
    }

    save_artifact(&results, "sec_env_override");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "env override bypass attempts: {} not blocked:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.description, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-5: Flag injection in denied commands.
/// e.g., `mcp-agent-mail share --verbose` should still be denied,
/// flags should not cause the denied command to be re-parsed as a serve flag.
#[test]
fn sec_flag_injection_in_denied_commands() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let injection_attempts: &[(&[&str], &str)] = &[
        (&["share", "--verbose"], "verbose flag with denied cmd"),
        (
            &["guard", "--host", "0.0.0.0"],
            "serve flags with denied cmd",
        ),
        (&["archive", "--port", "8080"], "port flag with denied cmd"),
        (&["share", "--no-tui"], "no-tui flag with denied cmd"),
        (&["doctor", "--", "serve"], "double-dash command escape"),
    ];

    let mut results = Vec::new();

    for (args, desc) in injection_attempts {
        let out = run_binary(&mcp, args);
        let exit = out.status.code();

        // Must still be denied (exit 2).
        let passed = exit == Some(2);

        results.push(SecurityRow {
            attack_class: "flag_injection".to_string(),
            description: desc.to_string(),
            input: args.join(" "),
            expected_behavior: "denied (exit 2)".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: vec![],
            passed,
        });
    }

    save_artifact(&results, "sec_flag_injection");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "flag injection bypass attempts: {} not blocked:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.input, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-6: Case sensitivity in denial gate.
/// `mcp-agent-mail Serve` or `SERVE` should NOT match the allowed `serve`.
/// clap is case-sensitive by default, so these should be caught by External.
#[test]
fn sec_case_sensitivity_denial() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let case_variants = &[
        ("Serve", "capitalized"),
        ("SERVE", "uppercase"),
        ("Config", "capitalized config"),
        ("CONFIG", "uppercase config"),
        ("sErVe", "mixed case"),
    ];

    let mut results = Vec::new();

    for (cmd, desc) in case_variants {
        let out = run_binary(&mcp, &[cmd]);
        let exit = out.status.code();

        // clap should parse these as External (unknown), leading to exit 2.
        let passed = exit == Some(2);

        results.push(SecurityRow {
            attack_class: "case_sensitivity".to_string(),
            description: desc.to_string(),
            input: cmd.to_string(),
            expected_behavior: "denied (exit 2)".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: vec![],
            passed,
        });
    }

    save_artifact(&results, "sec_case_sensitivity");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "case sensitivity bypass attempts: {} not blocked:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.input, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-7: Multiple denied commands in sequence should each be independently denied.
/// Ensures stateless denial (no state leakage between invocations).
#[test]
fn sec_stateless_denial_across_invocations() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // Run 3 denied commands in rapid succession.
    let commands = &["share", "guard", "doctor"];
    let mut results = Vec::new();

    for cmd in commands {
        let out = run_binary(&mcp, &[cmd]);
        let exit = out.status.code();
        let serr = String::from_utf8_lossy(&out.stderr).to_string();

        let passed = exit == Some(2) && serr.contains(cmd);
        results.push(SecurityRow {
            attack_class: "stateless_denial".to_string(),
            description: format!("sequential denial: {cmd}"),
            input: cmd.to_string(),
            expected_behavior: "denied (exit 2) with command name in stderr".to_string(),
            actual_exit_code: exit,
            actual_stderr_contains: if serr.contains(cmd) {
                vec![cmd.to_string()]
            } else {
                vec![]
            },
            passed,
        });
    }

    save_artifact(&results, "sec_stateless_denial");

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "stateless denial failures:\n{}",
        failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.input, r.actual_exit_code))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// SEC-8: Denial stderr must NOT leak internal paths or stack traces.
/// The denial message should be user-friendly, not debug output.
#[test]
fn sec_denial_no_internal_leakage() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let out = run_binary(&mcp, &["share"]);
    let serr = String::from_utf8_lossy(&out.stderr).to_string();

    // Should NOT contain:
    let forbidden_patterns = &[
        "panic",
        "thread 'main'",
        "stack backtrace",
        "RUST_BACKTRACE",
        "/src/main.rs",
        "unwrap()",
        "at /",
        "note: run with",
    ];

    let mut leaks = Vec::new();
    for pattern in forbidden_patterns {
        if serr.to_lowercase().contains(&pattern.to_lowercase()) {
            leaks.push(*pattern);
        }
    }

    let row = SecurityRow {
        attack_class: "info_leakage".to_string(),
        description: "denial stderr should not leak internals".to_string(),
        input: "share".to_string(),
        expected_behavior: "clean user-facing error message".to_string(),
        actual_exit_code: out.status.code(),
        actual_stderr_contains: if leaks.is_empty() {
            vec!["(clean)".to_string()]
        } else {
            leaks.iter().map(|s| s.to_string()).collect()
        },
        passed: leaks.is_empty(),
    };
    save_artifact(&row, "sec_no_leakage");

    assert!(
        leaks.is_empty(),
        "denial stderr leaks internal info: {:?}\nFull stderr:\n{}",
        leaks,
        serr
    );
}

/// SEC-9: Empty and whitespace-only commands are handled gracefully.
#[test]
fn sec_empty_and_whitespace_commands() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // Empty string as a command.
    let out = run_binary(&mcp, &[""]);
    let exit = out.status.code();
    // Should be denied or fail to parse, not exit 0.
    let passed = exit != Some(0);

    let row = SecurityRow {
        attack_class: "empty_input".to_string(),
        description: "empty string command".to_string(),
        input: "(empty)".to_string(),
        expected_behavior: "denied or parse error (exit != 0)".to_string(),
        actual_exit_code: exit,
        actual_stderr_contains: vec![],
        passed,
    };
    save_artifact(&row, "sec_empty_command");

    assert!(
        passed,
        "empty command should not be allowed (got exit {:?})",
        exit
    );
}

/// SEC-10: Verify denial exit code is always exactly 2 (not 1, not 127, not crash signal).
/// This is a contract: callers depend on exit 2 meaning "denied CLI command".
#[test]
fn sec_denial_exit_code_contract() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let commands = &[
        "share",
        "archive",
        "guard",
        "acks",
        "migrate",
        "list-projects",
        "clear-and-reset-everything",
        "doctor",
        "agents",
        "tooling",
        "macros",
        "contacts",
        "mail",
        "projects",
        "products",
        "file_reservations",
    ];

    let mut non_two = Vec::new();

    for cmd in commands {
        let out = run_binary(&mcp, &[cmd]);
        let exit = out.status.code();
        if exit != Some(2) {
            non_two.push(format!("{cmd} → exit {:?}", exit));
        }
    }

    save_artifact(
        &serde_json::json!({
            "test": "denial_exit_code_contract",
            "expected_exit": 2,
            "total_commands": commands.len(),
            "non_two_exits": non_two,
        }),
        "sec_exit_code_contract",
    );

    assert!(
        non_two.is_empty(),
        "denial exit code contract violations (expected 2 for all):\n  {}",
        non_two.join("\n  ")
    );
}

// ── Mode-Switch Tests (br-163x.5 / br-163x.6) ──────────────────────

/// MODE-1: MCP denial message includes both `am` and `AM_INTERFACE_MODE=cli`
/// remediation paths per SPEC-interface-mode-switch.md.
#[test]
fn mode_mcp_denial_includes_both_remediation_paths() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let out = run_binary(&mcp, &["share"]);
    let serr = String::from_utf8_lossy(&out.stderr).to_string();

    assert_eq!(out.status.code(), Some(2), "denial must exit 2");

    // Spec requires: "For operator CLI commands, use: am {command}"
    assert!(
        serr.contains("am share"),
        "denial must mention `am share` remediation, got:\n{serr}"
    );

    // Spec requires: "Or enable CLI mode: AM_INTERFACE_MODE=cli mcp-agent-mail {command} ..."
    assert!(
        serr.contains("AM_INTERFACE_MODE=cli"),
        "denial must mention AM_INTERFACE_MODE=cli remediation, got:\n{serr}"
    );
}

/// MODE-2: CLI mode via AM_INTERFACE_MODE=cli renders help with correct binary name.
#[test]
fn mode_cli_help_renders_with_mcp_agent_mail_name() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let out = run_binary_with_env(&mcp, &["--help"], &[("AM_INTERFACE_MODE", "cli")]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        out.status.code(),
        Some(0),
        "CLI mode --help must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("mcp-agent-mail"),
        "CLI mode help should reference mcp-agent-mail, got:\n{combined}"
    );
}

/// MODE-3: CLI mode denies MCP-only commands (serve) with exit 2.
/// Note: `config` is NOT denied because the CLI surface has its own config subcommand.
#[test]
fn mode_cli_denies_mcp_only_commands() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // `serve` is MCP-only and must be denied in CLI mode.
    let out = run_binary_with_env(&mcp, &["serve"], &[("AM_INTERFACE_MODE", "cli")]);
    let serr = String::from_utf8_lossy(&out.stderr).to_string();

    assert_eq!(
        out.status.code(),
        Some(2),
        "CLI mode must deny `serve` with exit 2, stderr:\n{serr}"
    );
    assert!(
        serr.contains("not available in CLI mode"),
        "CLI denial for `serve` must contain canonical phrase, got:\n{serr}"
    );
    assert!(
        serr.contains("AM_INTERFACE_MODE=cli"),
        "CLI denial for `serve` must reference current mode, got:\n{serr}"
    );

    // `config` exists in the CLI surface, so it should be allowed (exit 0).
    let out_config =
        run_binary_with_env(&mcp, &["config", "--help"], &[("AM_INTERFACE_MODE", "cli")]);
    assert_eq!(
        out_config.status.code(),
        Some(0),
        "CLI mode must allow `config --help` (it exists in CLI surface)"
    );
}

/// MODE-4: Invalid AM_INTERFACE_MODE value produces exit 2 with deterministic error.
#[test]
fn mode_invalid_value_exit_2() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let out = run_binary_with_env(&mcp, &["--help"], &[("AM_INTERFACE_MODE", "wat")]);
    let serr = String::from_utf8_lossy(&out.stderr).to_string();

    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid mode must exit 2, stderr:\n{serr}"
    );
    assert!(
        serr.contains("AM_INTERFACE_MODE"),
        "error must mention the env var, got:\n{serr}"
    );
}

/// MODE-5: CLI mode allows CLI-only commands that MCP mode denies.
#[test]
fn mode_cli_allows_cli_commands() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // These commands are denied in MCP mode but should be allowed in CLI mode.
    let cli_commands = &["share --help", "guard --help", "mail --help", "agents --help"];

    for entry in cli_commands {
        let args: Vec<&str> = entry.split_whitespace().collect();
        let out = run_binary_with_env(&mcp, &args, &[("AM_INTERFACE_MODE", "cli")]);

        assert_eq!(
            out.status.code(),
            Some(0),
            "CLI mode must allow `{entry}`, stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// MODE-6: Default (no AM_INTERFACE_MODE) is MCP mode — denies CLI commands.
#[test]
fn mode_default_is_mcp() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    // Explicitly unset to ensure default behavior.
    let mut cmd = std::process::Command::new(&mcp);
    cmd.arg("share")
        .env_remove("AM_INTERFACE_MODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().expect("spawn binary");

    assert_eq!(
        out.status.code(),
        Some(2),
        "default mode must deny CLI commands with exit 2"
    );
}

/// MODE-7: AM_INTERFACE_MODE=mcp explicitly is equivalent to default.
#[test]
fn mode_explicit_mcp_denies_cli() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    let out = run_binary_with_env(&mcp, &["share"], &[("AM_INTERFACE_MODE", "mcp")]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "explicit MCP mode must deny CLI commands with exit 2"
    );
}

/// MODE-8: AM_INTERFACE_MODE is case-insensitive.
#[test]
fn mode_case_insensitive() {
    let mcp = match mcp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MCP binary not found.");
            return;
        }
    };

    for val in &["CLI", "Cli", "cLi", " cli ", "MCP", "Mcp"] {
        let out = run_binary_with_env(&mcp, &["--help"], &[("AM_INTERFACE_MODE", val)]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "AM_INTERFACE_MODE={val:?} should be accepted (exit 0), stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
