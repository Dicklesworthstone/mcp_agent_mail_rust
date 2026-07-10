//! br-21gj.5.2: Integration + e2e matrix harness for MCP-deny vs CLI-allow.
//!
//! Tests that MCP binary denies CLI-only commands and CLI binary accepts them.
//! Produces structured log artifacts per test row.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn am_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_am").expect("CARGO_BIN_EXE_am must be set"))
}

fn explicit_mcp_bin_path() -> Option<PathBuf> {
    std::env::var_os("MCP_AGENT_MAIL_BIN").map(PathBuf::from)
}

fn implicit_mcp_bin_path() -> PathBuf {
    let am = am_bin();
    am.parent().expect("target dir").join("mcp-agent-mail")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn mcp_binary_freshness_inputs() -> [PathBuf; 2] {
    let root = repo_root();
    [
        root.join("crates/mcp-agent-mail/src/main.rs"),
        root.join("crates/mcp-agent-mail/Cargo.toml"),
    ]
}

fn path_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn implicit_mcp_bin_stale_reason(binary: &Path) -> Option<String> {
    let binary_modified = path_modified(binary)?;
    for source in mcp_binary_freshness_inputs() {
        let Some(source_modified) = path_modified(&source) else {
            continue;
        };
        if source_modified > binary_modified {
            return Some(format!(
                "MCP binary at {} is older than {}; build it with `cargo build -p mcp-agent-mail` or set MCP_AGENT_MAIL_BIN to a current binary",
                binary.display(),
                source.display()
            ));
        }
    }
    None
}

fn rch_worker_mcp_bin_candidates() -> Vec<PathBuf> {
    let implicit = implicit_mcp_bin_path();
    let Some(debug_dir) = implicit.parent() else {
        return Vec::new();
    };
    let Some(target_dir) = debug_dir.parent() else {
        return Vec::new();
    };
    let Some(target_name) = target_dir.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Some((worker_prefix, _job_suffix)) = target_name.split_once("-job-") else {
        return Vec::new();
    };
    let rch_prefix = format!("{worker_prefix}-job-");

    let mut candidates = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo_root()) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&rch_prefix) {
            continue;
        }
        let candidate = path.join("debug").join("mcp-agent-mail");
        if !candidate.is_file() {
            continue;
        }
        let modified = path_modified(&candidate).unwrap_or(UNIX_EPOCH);
        candidates.push((modified, candidate));
    }
    candidates.sort_by(|(left, _), (right, _)| right.cmp(left));
    candidates
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

fn implicit_mcp_bin_candidates() -> Vec<PathBuf> {
    std::iter::once(implicit_mcp_bin_path())
        .chain(rch_worker_mcp_bin_candidates())
        .collect()
}

fn resolve_mcp_bin_path() -> Result<PathBuf, String> {
    if let Some(explicit) = explicit_mcp_bin_path() {
        if explicit.is_file() {
            return Ok(explicit);
        }
        return Err(format!(
            "MCP binary not found at {}. Build with `cargo build -p mcp-agent-mail` or set MCP_AGENT_MAIL_BIN.",
            explicit.display()
        ));
    }

    let mut stale_reasons = Vec::new();
    let candidates = implicit_mcp_bin_candidates();
    for candidate in &candidates {
        if !candidate.is_file() {
            continue;
        }
        if let Some(reason) = implicit_mcp_bin_stale_reason(candidate) {
            stale_reasons.push(reason);
            continue;
        }
        return Ok(candidate.clone());
    }

    if let Some(reason) = stale_reasons.into_iter().next() {
        return Err(reason);
    }
    let searched = candidates
        .iter()
        .map(|candidate| candidate.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "MCP binary not found. Build with `cargo build -p mcp-agent-mail` or set MCP_AGENT_MAIL_BIN. Searched: {searched}"
    ))
}

fn mcp_bin_unavailable_reason() -> Option<String> {
    resolve_mcp_bin_path().err()
}

fn skip_if_mcp_bin_unavailable() -> bool {
    if let Some(reason) = mcp_bin_unavailable_reason() {
        eprintln!("SKIP: {reason}");
        true
    } else {
        false
    }
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
    // If the MCP binary isn't built yet, skip gracefully.
    let mcp_bin = match resolve_mcp_bin_path() {
        Ok(path) => path,
        Err(reason) => {
            return Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: reason.into_bytes(),
            };
        }
    };

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

fn isolated_env_without_precreated_root(label: &str) -> (PathBuf, Vec<(String, String)>) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "mode_matrix_{label}_{}_{}",
        std::process::id(),
        stamp
    ));
    let env = vec![
        (
            "DATABASE_URL".to_string(),
            format!("sqlite:///{}/test.sqlite3", root.display()),
        ),
        (
            "STORAGE_ROOT".to_string(),
            root.join("storage").display().to_string(),
        ),
        ("HOME".to_string(), root.join("home").display().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            root.join("xdg_config").display().to_string(),
        ),
        ("AGENT_NAME".to_string(), "TestAgent".to_string()),
        ("HTTP_HOST".to_string(), "127.0.0.1".to_string()),
        ("HTTP_PORT".to_string(), "1".to_string()),
        ("HTTP_PATH".to_string(), "/mcp/".to_string()),
    ];
    (root, env)
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
    &["capabilities", "--help"],
    &["agent", "--help"],
    &["status", "--help"],
    &["inbox", "--help"],
    &["reservations", "--help"],
    &["health", "--help"],
    &["thread", "--help"],
    &["check-inbox", "--help"],
    &["tui-dump", "--help"],
    &["check", "--help"],
    &["ci", "--help"],
    &["verify", "--help"],
    &["release", "--help"],
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
    &["atc", "--help"],
    &["robot", "--help"],
    &["robot-docs", "--help"],
    &["legacy", "--help"],
    &["upgrade", "--help"],
    &["service", "--help"],
    &["self-update", "--help"],
];

/// Commands that MCP binary should deny (exit code 2).
const MCP_DENY_COMMANDS: &[&[&str]] = &[
    &["share"],
    &["archive"],
    &["guard"],
    &["capabilities"],
    &["agent"],
    &["status"],
    &["inbox"],
    &["reservations"],
    &["health"],
    &["thread"],
    &["check-inbox"],
    &["tui-dump"],
    &["check"],
    &["ci"],
    &["verify"],
    &["release"],
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
    &["atc"],
    &["robot"],
    &["robot-docs"],
    &["legacy"],
    &["upgrade"],
    &["service"],
    &["self-update"],
];

/// Commands that MCP binary should allow (not deny).
const MCP_ALLOW_COMMANDS: &[&[&str]] = &[&["serve", "--help"], &["config"]];

struct CommandCorrectionCase {
    attempted: String,
    expected_cli: &'static str,
    expected_mcp_tool: Option<&'static str>,
}

fn mcp_name_mismatch_correction_cases() -> Vec<CommandCorrectionCase> {
    mcp_agent_mail_cli::mcp_tool_cli_corrections()
        .iter()
        .flat_map(|correction| {
            correction
                .attempted_names
                .iter()
                .map(move |attempted| CommandCorrectionCase {
                    attempted: (*attempted).to_string(),
                    expected_cli: correction.cli,
                    expected_mcp_tool: correction.mcp_tool,
                })
        })
        .collect()
}

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
    if skip_if_mcp_bin_unavailable() {
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
    if skip_if_mcp_bin_unavailable() {
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
    if skip_if_mcp_bin_unavailable() {
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

#[test]
fn matrix_mcp_name_mismatch_denials_print_exact_corrections() {
    let env = base_env();
    if skip_if_mcp_bin_unavailable() {
        return;
    }

    for case in mcp_name_mismatch_correction_cases() {
        let out = run_mcp(&[case.attempted.as_str()], &env);
        let sout = stdout_str(&out);
        let serr = stderr_str(&out);

        assert_eq!(
            out.status.code(),
            Some(2),
            "MCP name-mismatch denial for `{}` must exit 2.\nstderr:\n{}",
            case.attempted,
            serr
        );
        assert!(
            sout.is_empty(),
            "MCP name-mismatch denial for `{}` must keep stdout empty.\nstdout:\n{}",
            case.attempted,
            sout
        );
        assert!(
            serr.contains(&format!(
                "Error: \"{}\" is not an MCP server command.",
                case.attempted
            )),
            "denial must name attempted command `{}`.\nstderr:\n{}",
            case.attempted,
            serr
        );
        assert!(
            serr.contains("Corrected command:"),
            "denial for `{}` must include correction header.\nstderr:\n{}",
            case.attempted,
            serr
        );
        assert!(
            serr.contains(case.expected_cli),
            "denial for `{}` must include exact CLI correction `{}`.\nstderr:\n{}",
            case.attempted,
            case.expected_cli,
            serr
        );
        if let Some(expected_mcp_tool) = case.expected_mcp_tool {
            assert!(
                serr.contains(&format!("MCP tool: {expected_mcp_tool}")),
                "denial for `{}` must include MCP tool correction `{}`.\nstderr:\n{}",
                case.attempted,
                expected_mcp_tool,
                serr
            );
        }

        if case.expected_cli != format!("am {}", case.attempted) {
            assert!(
                !serr.contains(&format!("use: am {}", case.attempted)),
                "denial for `{}` must not emit the old misleading generic hint.\nstderr:\n{}",
                case.attempted,
                serr
            );
        }
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
    normalized
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
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
    if skip_if_mcp_bin_unavailable() {
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
fn golden_cli_mode_denial_for_mcp_only_serve() {
    let (root, mut env) = isolated_env_without_precreated_root("cli_deny_serve");
    env.push(("AM_INTERFACE_MODE".to_string(), "cli".to_string()));
    if skip_if_mcp_bin_unavailable() {
        return;
    }

    assert!(
        !root.exists(),
        "isolated root should not exist before CLI-mode wrong-surface denial: {}",
        root.display()
    );

    let out = run_mcp(&["serve"], &env);
    let serr = stderr_str(&out);

    assert_eq!(
        out.status.code(),
        Some(2),
        "CLI mode denial for `serve` must exit with code 2, got {:?}",
        out.status.code()
    );
    assert!(
        stdout_str(&out).is_empty(),
        "CLI mode denial for `serve` must not write to stdout"
    );
    assert!(
        !root.exists(),
        "CLI-mode wrong-surface denial must not create mailbox or config state under {}",
        root.display()
    );
    assert!(
        serr.contains("\"serve\" is not available in CLI mode"),
        "CLI mode denial must name the MCP-only command.\nActual stderr:\n{serr}"
    );
    assert!(
        serr.contains("unset AM_INTERFACE_MODE"),
        "CLI mode denial must explain how to return to MCP mode.\nActual stderr:\n{serr}"
    );
    assert!(
        serr.contains("mcp-agent-mail serve-http"),
        "CLI mode denial must list the CLI HTTP equivalent.\nActual stderr:\n{serr}"
    );

    let snapshot_name = "mcp_cli_mode_deny_serve.txt";
    maybe_update_golden_snapshot(snapshot_name, &serr);
    let golden = load_golden_snapshot(snapshot_name).unwrap_or_else(|| {
        panic!(
            "missing golden snapshot {snapshot_name}. \
             Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_cli_mode_denial_for_mcp_only_serve -- --nocapture` to generate it."
        )
    });
    let norm_golden = normalize_snapshot_text(&golden);
    let norm_actual = normalize_snapshot_text(&serr);
    assert_snapshot_match(
        "CLI mode denial 'serve'",
        &norm_golden,
        &norm_actual,
        "Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_cli_mode_denial_for_mcp_only_serve -- --nocapture` to update.",
    );
}

#[test]
fn golden_mcp_mode_denial_for_cli_only_doctor_has_no_side_effects() {
    let (root, env) = isolated_env_without_precreated_root("mcp_deny_doctor");
    if skip_if_mcp_bin_unavailable() {
        return;
    }

    assert!(
        !root.exists(),
        "isolated root should not exist before wrong-surface denial: {}",
        root.display()
    );

    let out = run_mcp(&["doctor"], &env);
    let serr = stderr_str(&out);

    assert_eq!(
        out.status.code(),
        Some(2),
        "MCP mode denial for CLI-only `doctor` must exit with code 2, got {:?}",
        out.status.code()
    );
    assert!(
        stdout_str(&out).is_empty(),
        "MCP mode denial for CLI-only `doctor` must not write to stdout"
    );
    assert!(
        !root.exists(),
        "wrong-surface denial must not create mailbox or config state under {}",
        root.display()
    );

    let snapshot_name = "mcp_deny_doctor.txt";
    maybe_update_golden_snapshot(snapshot_name, &serr);
    let golden = load_golden_snapshot(snapshot_name).unwrap_or_else(|| {
        panic!(
            "missing golden snapshot {snapshot_name}. \
             Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_mcp_mode_denial_for_cli_only_doctor_has_no_side_effects -- --nocapture` to generate it."
        )
    });
    let norm_golden = normalize_snapshot_text(&golden);
    let norm_actual = normalize_snapshot_text(&serr);
    assert_snapshot_match(
        "MCP mode denial 'doctor'",
        &norm_golden,
        &norm_actual,
        "Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_mcp_mode_denial_for_cli_only_doctor_has_no_side_effects -- --nocapture` to update.",
    );
}

#[test]
fn golden_mcp_mode_denial_for_setup_and_robot_has_no_side_effects() -> Result<(), String> {
    let cases = [
        ("setup", "mcp_deny_setup.txt"),
        ("robot", "mcp_deny_robot.txt"),
    ];
    if skip_if_mcp_bin_unavailable() {
        return Ok(());
    }

    for (command, snapshot_name) in cases {
        let (root, env) = isolated_env_without_precreated_root(command);

        assert!(
            !root.exists(),
            "isolated root should not exist before wrong-surface denial for {command}: {}",
            root.display()
        );

        let out = run_mcp(&[command], &env);
        let serr = stderr_str(&out);

        assert_eq!(
            out.status.code(),
            Some(2),
            "MCP mode denial for CLI-only `{command}` must exit with code 2, got {:?}",
            out.status.code()
        );
        assert!(
            stdout_str(&out).is_empty(),
            "MCP mode denial for CLI-only `{command}` must not write to stdout"
        );
        assert!(
            !root.exists(),
            "wrong-surface `{command}` denial must not create mailbox or config state under {}",
            root.display()
        );

        maybe_update_golden_snapshot(snapshot_name, &serr);
        let golden = load_golden_snapshot(snapshot_name).ok_or_else(|| {
            format!(
                "missing golden snapshot {snapshot_name}. \
                 Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_mcp_mode_denial_for_setup_and_robot_has_no_side_effects -- --nocapture` to generate it."
            )
        })?;
        let norm_golden = normalize_snapshot_text(&golden);
        let norm_actual = normalize_snapshot_text(&serr);
        assert_snapshot_match(
            &format!("MCP mode denial '{command}'"),
            &norm_golden,
            &norm_actual,
            "Run `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli golden_mcp_mode_denial_for_setup_and_robot_has_no_side_effects -- --nocapture` to update.",
        );
    }

    Ok(())
}

#[test]
fn cli_mode_opt_in_allows_cli_help_and_invalid_mode_is_side_effect_free() -> Result<(), String> {
    if skip_if_mcp_bin_unavailable() {
        return Ok(());
    }

    let (cli_root, mut cli_env) = isolated_env_without_precreated_root("cli_opt_in");
    cli_env.push(("AM_INTERFACE_MODE".to_string(), "cli".to_string()));

    let cli_cases: &[(&[&str], &str)] = &[
        (&["--help"], "mcp-agent-mail"),
        (&["share", "--help"], "Usage:"),
    ];
    for (args, required_stdout) in cli_cases {
        let out = run_mcp(args, &cli_env);
        let sout = stdout_str(&out);

        assert_eq!(
            out.status.code(),
            Some(0),
            "CLI opt-in command {:?} must exit 0, got {:?}",
            args,
            out.status.code()
        );
        assert!(
            sout.contains(required_stdout),
            "CLI opt-in command {:?} stdout must contain {required_stdout:?}.\nActual stdout:\n{sout}",
            args
        );
    }
    assert!(
        !cli_root.exists(),
        "CLI opt-in help commands must not create mailbox or config state under {}",
        cli_root.display()
    );

    let (invalid_root, mut invalid_env) =
        isolated_env_without_precreated_root("invalid_interface_mode");
    invalid_env.push(("AM_INTERFACE_MODE".to_string(), "wat".to_string()));

    let out = run_mcp(&["--help"], &invalid_env);
    let serr = stderr_str(&out);
    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid AM_INTERFACE_MODE must exit with usage code 2, got {:?}",
        out.status.code()
    );
    assert!(
        stdout_str(&out).is_empty(),
        "invalid AM_INTERFACE_MODE must not write to stdout"
    );
    assert!(
        serr.contains("AM_INTERFACE_MODE") && serr.contains("wat"),
        "invalid mode denial must name AM_INTERFACE_MODE and the rejected value.\nActual stderr:\n{serr}"
    );
    assert!(
        !invalid_root.exists(),
        "invalid AM_INTERFACE_MODE denial must not create mailbox or config state under {}",
        invalid_root.display()
    );

    Ok(())
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
        (&["service", "--help"], "cli_help_service.txt"),
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

#[test]
fn docs_dual_mode_guidance_matches_mode_matrix_contract() {
    let readme = include_str!("../../../README.md");
    let runbook = include_str!("../../../docs/OPERATOR_RUNBOOK.md");
    let spec = include_str!("../../../docs/SPEC-interface-mode-switch.md");

    assert!(
        readme.contains("mcp-agent-mail")
            && readme.contains("stdio transport")
            && readme.contains("AM_INTERFACE_MODE=cli mcp-agent-mail"),
        "README must document MCP stdio default and CLI opt-in mode"
    );
    assert!(
        runbook.contains("MCP server: `mcp-agent-mail`")
            && runbook.contains("CLI: `am`")
            && runbook.contains("deny on `stderr` and exit with code `2`"),
        "operator runbook must preserve the CLI-vs-server wrong-surface guidance"
    );
    assert!(
        spec.contains("For operator CLI commands, use: am {command}")
            && spec.contains("AM_INTERFACE_MODE=wat mcp-agent-mail --help"),
        "interface mode spec must preserve denial remediation and invalid-mode test vector"
    );
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
