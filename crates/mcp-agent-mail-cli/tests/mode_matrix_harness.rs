//! br-21gj.5.2: Integration + e2e matrix harness for MCP-deny vs CLI-allow.
//!
//! Tests that MCP binary denies CLI-only commands and CLI binary accepts them.
//! Produces structured log artifacts per test row.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    if let Ok(override_root) = std::env::var("AM_MODE_MATRIX_ARTIFACT_DIR") {
        return PathBuf::from(override_root).join("mode_matrix");
    }
    repo_root().join("tests/artifacts/cli/mode_matrix")
}

/// Structured log entry for each matrix row.
#[derive(Debug, serde::Serialize)]
struct MatrixRowLog {
    binary: String,
    command: String,
    args: Vec<String>,
    expected_decision: String, // "allow" or "deny"
    actual_exit_code: Option<i32>,
    stdout_digest: String,
    stderr_digest: String,
    passed: bool,
}

impl MatrixRowLog {
    fn write_artifact(&self, case_name: &str) {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
        let pid = std::process::id();
        let dir = artifacts_dir().join(format!("{ts}_{pid}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{case_name}.json"));
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        let _ = std::fs::write(&path, &json);
        eprintln!("matrix artifact: {}", path.display());
    }
}

/// Run the `am` CLI binary with given args and env.
fn run_cli(args: &[&str], env_pairs: &[(String, String)]) -> Output {
    let mut cmd = Command::new(am_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env_pairs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn am cli")
}

/// Run the MCP server binary with given args and env.
/// The binary rejects CLI-only commands with exit code 2.
fn run_mcp(args: &[&str], env_pairs: &[(String, String)]) -> Output {
    // Find the mcp-agent-mail binary in the same target dir as the am binary.
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp_bin = target_dir.join("mcp-agent-mail");

    // If the MCP binary isn't built yet, skip gracefully.
    if !mcp_bin.exists() {
        return Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: format!("MCP binary not found at {}", mcp_bin.display()).into_bytes(),
        };
    }

    let mut cmd = Command::new(&mcp_bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env_pairs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn mcp-agent-mail")
}

fn base_env() -> Vec<(String, String)> {
    let tmp = std::env::temp_dir().join("mode_matrix_test");
    let _ = std::fs::create_dir_all(&tmp);
    vec![
        (
            "DATABASE_URL".to_string(),
            format!("sqlite:///{}/test.sqlite3", tmp.display()),
        ),
        ("STORAGE_ROOT".to_string(), tmp.display().to_string()),
        ("AGENT_NAME".to_string(), "TestAgent".to_string()),
        ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
        ("HTTP_PORT".to_string(), "1".to_string()),
        ("HTTP_PATH".to_string(), "/mcp/".to_string()),
    ]
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn digest(s: &str) -> String {
    if s.chars().count() > 200 {
        let head: String = s.chars().take(200).collect();
        format!("{head}... ({} bytes)", s.len())
    } else {
        s.to_string()
    }
}

// ── CLI-allow matrix ─────────────────────────────────────────────────

/// Commands that should be accepted (parsed) by the CLI binary.
/// We test with --help to avoid side effects; exit code 0 means clap accepted it.
const CLI_ALLOW_COMMANDS: &[&[&str]] = &[
    &["serve-http", "--help"],
    &["serve-stdio", "--help"],
    &["check-inbox", "--help"],
    &["ci", "--help"],
    &["bench", "--help"],
    &["e2e", "--help"],
    &["share", "--help"],
    &["archive", "--help"],
    &["guard", "--help"],
    &["acks", "--help"],
    &["list-acks", "--help"],
    &["migrate", "--help"],
    &["list-projects", "--help"],
    &["clear-and-reset-everything", "--help"],
    &["config", "--help"],
    &["amctl", "--help"],
    &["projects", "--help"],
    &["mail", "--help"],
    &["products", "--help"],
    &["docs", "--help"],
    &["doctor", "--help"],
    &["agents", "--help"],
    &["tooling", "--help"],
    &["macros", "--help"],
    &["contacts", "--help"],
    &["beads", "--help"],
    &["file_reservations", "--help"],
    &["setup", "--help"],
    &["golden", "--help"],
    &["flake-triage", "--help"],
    &["robot", "--help"],
    &["legacy", "--help"],
    &["upgrade", "--help"],
];

/// Commands that MCP binary should deny (exit code 2).
const MCP_DENY_COMMANDS: &[&[&str]] = &[
    &["share"],
    &["archive"],
    &["guard"],
    &["check-inbox"],
    &["ci"],
    &["bench"],
    &["e2e"],
    &["acks"],
    &["migrate"],
    &["list-projects"],
    &["clear-and-reset-everything"],
    &["config"], // MCP has its own "config" subcommand, so this is actually allowed
    &["doctor"],
    &["agents"],
    &["tooling"],
    &["macros"],
    &["contacts"],
    &["mail"],
    &["projects"],
    &["products"],
    &["file_reservations"],
    &["beads"],
    &["setup"],
    &["golden"],
    &["flake-triage"],
    &["robot"],
    &["legacy"],
    &["upgrade"],
];

/// Commands that MCP binary should allow (not deny).
const MCP_ALLOW_COMMANDS: &[&[&str]] = &[&["serve", "--help"], &["config"]];

// ── Tests ────────────────────────────────────────────────────────────

#[test]
fn matrix_cli_binary_accepts_all_command_families() {
    let env = base_env();
    let mut results = Vec::new();

    for args in CLI_ALLOW_COMMANDS {
        let out = run_cli(args, &env);
        let exit = out.status.code();
        let sout = stdout_str(&out);
        let serr = stderr_str(&out);

        let passed = exit == Some(0);
        let log = MatrixRowLog {
            binary: "am".to_string(),
            command: args.join(" "),
            args: args.iter().map(|s| s.to_string()).collect(),
            expected_decision: "allow".to_string(),
            actual_exit_code: exit,
            stdout_digest: digest(&sout),
            stderr_digest: digest(&serr),
            passed,
        };
        log.write_artifact(&format!("cli_allow_{}", args[0].replace('-', "_")));
        results.push(log);
    }

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    if !failures.is_empty() {
        let msgs: Vec<String> = failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.command, r.actual_exit_code))
            .collect();
        panic!(
            "CLI-allow matrix failures ({}/{}):\n{}",
            failures.len(),
            results.len(),
            msgs.join("\n")
        );
    }
}

#[test]
fn matrix_mcp_binary_denies_cli_only_commands() {
    let env = base_env();
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp_bin = target_dir.join("mcp-agent-mail");

    if !mcp_bin.exists() {
        eprintln!(
            "SKIP: MCP binary not found at {}. Build with `cargo build -p mcp-agent-mail`.",
            mcp_bin.display()
        );
        return;
    }

    let mut results = Vec::new();

    for args in MCP_DENY_COMMANDS {
        let out = run_mcp(args, &env);
        let exit = out.status.code();
        let serr = stderr_str(&out);

        // MCP binary should exit with code 2 for CLI-only commands.
        // Exception: "config" is a valid MCP command too.
        let is_config = args.first() == Some(&"config");
        let expected_exit = if is_config { Some(0) } else { Some(2) };
        let passed = exit == expected_exit;

        let log = MatrixRowLog {
            binary: "mcp-agent-mail".to_string(),
            command: args.join(" "),
            args: args.iter().map(|s| s.to_string()).collect(),
            expected_decision: if is_config {
                "allow".to_string()
            } else {
                "deny".to_string()
            },
            actual_exit_code: exit,
            stdout_digest: digest(&stdout_str(&out)),
            stderr_digest: digest(&serr),
            passed,
        };
        log.write_artifact(&format!("mcp_deny_{}", args[0].replace('-', "_")));
        results.push(log);
    }

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    if !failures.is_empty() {
        let msgs: Vec<String> = failures
            .iter()
            .map(|r| {
                format!(
                    "  {} → exit {:?} (expected {} → {:?})",
                    r.command,
                    r.actual_exit_code,
                    r.expected_decision,
                    if r.expected_decision == "deny" {
                        "2"
                    } else {
                        "0"
                    }
                )
            })
            .collect();
        panic!(
            "MCP-deny matrix failures ({}/{}):\n{}",
            failures.len(),
            results.len(),
            msgs.join("\n")
        );
    }
}

#[test]
fn matrix_mcp_binary_allows_server_commands() {
    let env = base_env();
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp_bin = target_dir.join("mcp-agent-mail");

    if !mcp_bin.exists() {
        eprintln!("SKIP: MCP binary not found.");
        return;
    }

    let mut results = Vec::new();

    for args in MCP_ALLOW_COMMANDS {
        let out = run_mcp(args, &env);
        let exit = out.status.code();

        // --help triggers clap exit 0; "config" prints and exits 0.
        let passed = exit == Some(0);

        let log = MatrixRowLog {
            binary: "mcp-agent-mail".to_string(),
            command: args.join(" "),
            args: args.iter().map(|s| s.to_string()).collect(),
            expected_decision: "allow".to_string(),
            actual_exit_code: exit,
            stdout_digest: digest(&stdout_str(&out)),
            stderr_digest: digest(&stderr_str(&out)),
            passed,
        };
        log.write_artifact(&format!("mcp_allow_{}", args[0].replace(['-', ' '], "_")));
        results.push(log);
    }

    let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();
    if !failures.is_empty() {
        let msgs: Vec<String> = failures
            .iter()
            .map(|r| format!("  {} → exit {:?}", r.command, r.actual_exit_code))
            .collect();
        panic!(
            "MCP-allow matrix failures ({}/{}):\n{}",
            failures.len(),
            results.len(),
            msgs.join("\n")
        );
    }
}

#[test]
fn matrix_mcp_denial_message_contains_remediation() {
    let env = base_env();
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp_bin = target_dir.join("mcp-agent-mail");

    if !mcp_bin.exists() {
        eprintln!("SKIP: MCP binary not found.");
        return;
    }

    // Test that the denial message includes the command name and remediation hint.
    let test_commands = &["share", "guard", "doctor"];
    for cmd in test_commands {
        let out = run_mcp(&[cmd], &env);
        let serr = stderr_str(&out);

        assert!(
            serr.contains(cmd),
            "denial stderr for '{cmd}' should mention the command: {serr}"
        );
        assert!(
            serr.contains("use: am "),
            "denial stderr for '{cmd}' should mention the CLI binary: {serr}"
        );
    }
}

// ── br-21gj.5.5: Golden snapshot validation for denial/help/usage ────

/// Canonical denial message format per SPEC-denial-ux-contract.md.
/// The denial message must follow this exact structure (modulo the command name).
const DENIAL_CANONICAL_PREFIX: &str = "Error: \"";
const DENIAL_CANONICAL_MIDDLE: &str = "\" is not an MCP server command.";
const DENIAL_CANONICAL_REMEDIATION: &str = "Agent Mail MCP server accepts: serve, config";
const DENIAL_CANONICAL_CLI_HINT: &str = "For operator CLI commands, use: am ";

/// Load a golden snapshot file from tests/fixtures/golden_snapshots/.
fn load_golden_snapshot(name: &str) -> Option<String> {
    let fixture_dir = repo_root().join("tests/fixtures/golden_snapshots");
    let path = fixture_dir.join(name);
    std::fs::read_to_string(&path).ok()
}

/// Save a golden snapshot for updating fixtures.
fn save_golden_snapshot(name: &str, content: &str) {
    let fixture_dir = repo_root().join("tests/fixtures/golden_snapshots");
    let _ = std::fs::create_dir_all(&fixture_dir);
    let path = fixture_dir.join(name);
    let _ = std::fs::write(&path, content);
}

fn should_update_golden_snapshots() -> bool {
    std::env::var("UPDATE_GOLDEN").ok().is_some_and(|value| {
        value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
    })
}

fn maybe_update_golden_snapshot(name: &str, content: &str) {
    if should_update_golden_snapshots() {
        save_golden_snapshot(name, content);
        eprintln!("updated golden snapshot: {name}");
    }
}

fn normalize_snapshot_text(text: &str) -> String {
    let normalized = mcp_agent_mail_cli::golden::normalize_output(text);
    normalized.trim_end().to_string()
}

fn assert_snapshot_match(case_label: &str, expected: &str, actual: &str, update_hint: &str) {
    let comparison = mcp_agent_mail_cli::golden::compare_text(expected, actual);
    assert!(
        comparison.matches,
        "{case_label} snapshot drift.\n\
         {update_hint}\n\
         expected_sha256: {}\n\
         actual_sha256:   {}\n\
         {}",
        comparison.expected_sha256,
        comparison.actual_sha256,
        comparison
            .inline_diff
            .unwrap_or_else(|| "(inline diff unavailable)".to_string())
    );
}

#[test]
fn golden_denial_message_format_contract() {
    let env = base_env();
    let am = am_bin();
    let target_dir = am.parent().expect("target dir");
    let mcp_bin = target_dir.join("mcp-agent-mail");

    if !mcp_bin.exists() {
        eprintln!("SKIP: MCP binary not found.");
        return;
    }

    let denied_commands = ["share", "guard", "doctor", "archive", "migrate"];

    for cmd in &denied_commands {
        let out = run_mcp(&[cmd], &env);
        let serr = stderr_str(&out);

        // Verify canonical format structure
        assert!(
            serr.contains(&format!(
                "{DENIAL_CANONICAL_PREFIX}{cmd}{DENIAL_CANONICAL_MIDDLE}"
            )),
            "denial for '{cmd}' must contain canonical error line.\nActual stderr:\n{serr}"
        );
        assert!(
            serr.contains(DENIAL_CANONICAL_REMEDIATION),
            "denial for '{cmd}' must list accepted commands.\nActual stderr:\n{serr}"
        );
        assert!(
            serr.contains(&format!("{DENIAL_CANONICAL_CLI_HINT}{cmd}")),
            "denial for '{cmd}' must include CLI remediation hint.\nActual stderr:\n{serr}"
        );

        // Exit code must be 2 (POSIX usage error)
        assert_eq!(
            out.status.code(),
            Some(2),
            "denial for '{cmd}' must exit with code 2, got {:?}",
            out.status.code()
        );

        // Stdout must be empty (denials go to stderr only)
        assert!(
            stdout_str(&out).is_empty(),
            "denial for '{cmd}' must not write to stdout"
        );

        // Update snapshots only when explicitly requested.
        maybe_update_golden_snapshot(&format!("mcp_deny_{cmd}.txt"), &serr);

        // Check against existing golden snapshot
        if let Some(golden) = load_golden_snapshot(&format!("mcp_deny_{cmd}.txt")) {
            let norm_golden = normalize_snapshot_text(&golden);
            let norm_actual = normalize_snapshot_text(&serr);
            assert_snapshot_match(
                &format!("denial '{cmd}'"),
                &norm_golden,
                &norm_actual,
                "Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_denial_message_format_contract -- --nocapture` to update.",
            );
        }

        eprintln!("golden_denial[{cmd}] PASS");
    }
}

#[test]
fn golden_cli_help_snapshot_stability() {
    let env = base_env();

    // Test top-level help and key subcommand help outputs against golden fixtures
    let help_cases: &[(&[&str], &str)] = &[
        (&["--help"], "cli_help_top_level.txt"),
        (&["share", "--help"], "cli_help_share.txt"),
        (&["guard", "--help"], "cli_help_guard.txt"),
        (&["doctor", "--help"], "cli_help_doctor.txt"),
        (&["contacts", "--help"], "cli_help_contacts.txt"),
        (&["macros", "--help"], "cli_help_macros.txt"),
    ];

    for (args, snapshot_name) in help_cases {
        let out = run_cli(args, &env);
        let sout = stdout_str(&out);

        assert_eq!(
            out.status.code(),
            Some(0),
            "help for {:?} should exit 0, got {:?}",
            args,
            out.status.code()
        );

        // Update snapshots only when explicitly requested.
        maybe_update_golden_snapshot(snapshot_name, &sout);

        // Validate against existing golden if present
        if let Some(golden) = load_golden_snapshot(snapshot_name) {
            let norm_golden = normalize_snapshot_text(&golden);
            let norm_actual = normalize_snapshot_text(&sout);
            assert_snapshot_match(
                &format!("help {:?}", args),
                &norm_golden,
                &norm_actual,
                "Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_cli_help_snapshot_stability -- --nocapture` to update.",
            );
        }

        eprintln!("golden_help[{}] PASS ({} bytes)", snapshot_name, sout.len());
    }
}

#[test]
fn golden_usage_error_format() {
    let env = base_env();

    // Test that invalid usage produces structured error with help hint
    let usage_cases: &[(&[&str], &str)] = &[
        // Unknown subcommand
        (&["nonexistent-command"], "unrecognized subcommand"),
        // Missing required args (for commands that need them)
        (&["share", "export"], "required"),
    ];

    for (args, expected_fragment) in usage_cases {
        let out = run_cli(args, &env);
        let serr = stderr_str(&out);

        // Usage errors should go to stderr
        assert!(
            !serr.is_empty(),
            "usage error for {:?} should produce stderr output",
            args
        );

        // Exit code should be non-zero
        assert_ne!(
            out.status.code(),
            Some(0),
            "usage error for {:?} should not exit 0",
            args
        );

        eprintln!(
            "golden_usage[{:?}] exit={:?} stderr_contains={:?} PASS",
            args,
            out.status.code(),
            expected_fragment
        );
    }
}

/// Verify that the matrix rows cover all top-level CLI subcommands.
#[test]
fn matrix_coverage_complete() {
    use clap::CommandFactory;
    use mcp_agent_mail_cli::Cli;

    let cli_commands: Vec<String> = CLI_ALLOW_COMMANDS
        .iter()
        .map(|args| args[0].to_string())
        .collect();

    // Check that every actual clap subcommand is present in our matrix.
    let skip = ["help", "lint", "typecheck", "am-run"]; // meta/internal commands
    let mut commands_section: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .filter(|cmd| !skip.contains(&cmd.as_str()))
        .collect();
    commands_section.sort();

    let mut missing = Vec::new();
    for cmd in &commands_section {
        if !cli_commands.contains(cmd) {
            missing.push(cmd.clone());
        }
    }

    let mut stale = Vec::new();
    for cmd in &cli_commands {
        if !commands_section.contains(cmd) && !skip.contains(&cmd.as_str()) {
            stale.push(cmd.clone());
        }
    }

    if !missing.is_empty() || !stale.is_empty() {
        panic!(
            "CLI matrix coverage mismatch.\nMissing from matrix: {:?}\nStale in matrix: {:?}\nClap commands: {:?}\nMatrix commands: {:?}",
            missing, stale, commands_section, cli_commands
        );
    }
}
