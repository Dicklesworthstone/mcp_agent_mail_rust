//! br-1ah9d: Native-vs-legacy performance guardrails for migrated command surfaces.
//!
//! This suite enforces per-surface latency budgets for native commands and,
//! where compatibility shims still exist, compares native p95 against legacy
//! p95 with explicit delta budgets. If a legacy script has been removed, the
//! artifact records deterministic "legacy unavailable" rationale.

#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const DEFAULT_ITERATIONS: usize = 8;

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
    repo_root().join("tests/artifacts/cli/perf_guardrails")
}

#[derive(Debug, serde::Serialize)]
struct EnvironmentProfile {
    os: String,
    arch: String,
    family: String,
    cpu_count: usize,
    rust_pkg_version: String,
}

#[derive(Debug, serde::Serialize)]
struct GuardrailCaseResult {
    surface_id: String,
    native_command: String,
    legacy_command: Option<String>,
    iterations: usize,
    native_samples_us: Vec<u64>,
    native_p95_us: Option<u64>,
    native_budget_p95_us: u64,
    native_budget_ok: bool,
    native_error: Option<String>,
    legacy_status: String,
    legacy_unavailable_reason: Option<String>,
    legacy_samples_us: Option<Vec<u64>>,
    legacy_p95_us: Option<u64>,
    legacy_error: Option<String>,
    delta_p95_us: Option<i64>,
    max_native_delta_p95_us: Option<u64>,
    delta_ok: Option<bool>,
    passed: bool,
}

#[derive(Debug, serde::Serialize)]
struct GuardrailSuiteArtifact {
    schema_version: u32,
    run_ts: String,
    iterations: usize,
    environment: EnvironmentProfile,
    cases: Vec<GuardrailCaseResult>,
}

#[derive(Debug, Clone, Copy)]
enum LegacyCommand {
    Script {
        rel_path: &'static str,
        args: &'static [&'static str],
        unavailable_reason: &'static str,
    },
    Unavailable {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
struct GuardrailCase {
    surface_id: &'static str,
    native_args: &'static [&'static str],
    native_budget_p95_us: u64,
    max_native_delta_p95_us: Option<u64>,
    legacy: LegacyCommand,
}

fn environment_profile() -> EnvironmentProfile {
    EnvironmentProfile {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        family: std::env::consts::FAMILY.to_string(),
        cpu_count: std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(0),
        rust_pkg_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn read_u64_env(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse::<u64>().ok()
}

fn metric_env_suffix(metric_name: &str) -> String {
    metric_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn read_surface_u64(prefix: &str, surface_id: &str) -> Option<u64> {
    let suffix = metric_env_suffix(surface_id);
    let metric_specific = format!("{prefix}_{suffix}");
    read_u64_env(&metric_specific).or_else(|| read_u64_env(prefix))
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn format_command(program: &str, args: &[&str]) -> String {
    if args.is_empty() {
        return program.to_string();
    }
    format!("{program} {}", args.join(" "))
}

fn build_path_env(binary_dir: &Path) -> Option<std::ffi::OsString> {
    let mut entries = vec![binary_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(entries).ok()
}

fn run_command_samples(
    program: &Path,
    args: &[&str],
    iterations: usize,
    binary_dir: &Path,
) -> Result<Vec<u64>, String> {
    let path_env = build_path_env(binary_dir);
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(path) = path_env.as_ref() {
            cmd.env("PATH", path);
        }

        let started = Instant::now();
        let output = cmd
            .output()
            .map_err(|err| format!("failed to spawn '{}': {err}", program.display()))?;
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first_line = stderr.lines().next().unwrap_or("").trim();
            return Err(format!(
                "command '{}' exited {:?} ({first_line})",
                program.display(),
                output.status.code()
            ));
        }
        samples.push(elapsed_us);
    }
    Ok(samples)
}

fn samples_p95(samples_us: &[u64]) -> Option<u64> {
    if samples_us.is_empty() {
        return None;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    Some(percentile(&sorted, 95.0))
}

fn save_artifact(artifact: &GuardrailSuiteArtifact) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S%.3fZ").to_string();
    let dir = artifacts_dir().join(format!("{ts}_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("perf_guardrails.json");
    if let Ok(json) = serde_json::to_string_pretty(artifact) {
        let _ = std::fs::write(&path, json);
        eprintln!("artifact: {}", path.display());
    }

    let trends_dir = artifacts_dir().join("trends");
    let _ = std::fs::create_dir_all(&trends_dir);
    let trend_path = trends_dir.join("perf_guardrails_timeseries.jsonl");
    if let Ok(mut fh) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trend_path)
    {
        for row in &artifact.cases {
            let trend_row = serde_json::json!({
                "schema_version": artifact.schema_version,
                "run_ts": artifact.run_ts,
                "surface_id": row.surface_id,
                "native_command": row.native_command,
                "legacy_command": row.legacy_command,
                "iterations": row.iterations,
                "native_p95_us": row.native_p95_us,
                "legacy_p95_us": row.legacy_p95_us,
                "delta_p95_us": row.delta_p95_us,
                "native_budget_p95_us": row.native_budget_p95_us,
                "max_native_delta_p95_us": row.max_native_delta_p95_us,
                "native_budget_ok": row.native_budget_ok,
                "delta_ok": row.delta_ok,
                "legacy_status": row.legacy_status,
                "legacy_unavailable_reason": row.legacy_unavailable_reason,
                "passed": row.passed,
            });
            if let Ok(line) = serde_json::to_string(&trend_row) {
                let _ = writeln!(fh, "{line}");
            }
        }
        eprintln!("artifact: {}", trend_path.display());
    }
}

fn guardrail_cases() -> &'static [GuardrailCase] {
    &[
        GuardrailCase {
            surface_id: "ci_help",
            native_args: &["ci", "--help"],
            native_budget_p95_us: 400_000,
            max_native_delta_p95_us: Some(120_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/ci.sh",
                args: &["--help"],
                unavailable_reason: "legacy ci.sh script removed after native cutover; compare via historical artifacts/baselines",
            },
        },
        GuardrailCase {
            surface_id: "bench_help",
            native_args: &["bench", "--help"],
            native_budget_p95_us: 400_000,
            max_native_delta_p95_us: Some(120_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/bench_cli.sh",
                args: &["--help"],
                unavailable_reason: "legacy bench_cli.sh removed after native cutover; compare via historical artifacts/baselines",
            },
        },
        GuardrailCase {
            surface_id: "golden_verify_help",
            native_args: &["golden", "verify", "--help"],
            native_budget_p95_us: 400_000,
            max_native_delta_p95_us: Some(120_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/bench_golden.sh",
                args: &["--help"],
                unavailable_reason: "legacy bench_golden.sh removed after native cutover; compare via historical artifacts/baselines",
            },
        },
        GuardrailCase {
            surface_id: "flake_triage_help",
            native_args: &["flake-triage", "--help"],
            native_budget_p95_us: 450_000,
            max_native_delta_p95_us: Some(140_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/flake_triage.sh",
                args: &["--help"],
                unavailable_reason: "legacy flake_triage.sh removed after native cutover; compare via historical artifacts/baselines",
            },
        },
        GuardrailCase {
            surface_id: "check_inbox_help",
            native_args: &["check-inbox", "--help"],
            native_budget_p95_us: 450_000,
            max_native_delta_p95_us: Some(180_000),
            legacy: LegacyCommand::Script {
                rel_path: "legacy/hooks/check_inbox.sh",
                args: &["--help"],
                unavailable_reason: "legacy hook script missing; cannot run direct native-vs-legacy comparison",
            },
        },
        GuardrailCase {
            surface_id: "serve_http_help",
            native_args: &["serve-http", "--help"],
            native_budget_p95_us: 500_000,
            max_native_delta_p95_us: Some(220_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/am",
                args: &["--help"],
                unavailable_reason: "legacy scripts/am wrapper missing; cannot compare convenience wrapper overhead",
            },
        },
        GuardrailCase {
            surface_id: "e2e_run_help",
            native_args: &["e2e", "run", "--help"],
            native_budget_p95_us: 500_000,
            max_native_delta_p95_us: Some(240_000),
            legacy: LegacyCommand::Script {
                rel_path: "scripts/e2e_test.sh",
                args: &["--help"],
                unavailable_reason: "legacy e2e_test.sh shim missing; cannot compare wrapper overhead",
            },
        },
        GuardrailCase {
            surface_id: "share_wizard_help",
            native_args: &["share", "wizard", "--help"],
            native_budget_p95_us: 500_000,
            max_native_delta_p95_us: None,
            legacy: LegacyCommand::Unavailable {
                reason: "legacy surface was an E2E script harness, not a direct CLI parity wrapper",
            },
        },
        GuardrailCase {
            surface_id: "share_deploy_verify_live_help",
            native_args: &["share", "deploy", "verify-live", "--help"],
            native_budget_p95_us: 500_000,
            max_native_delta_p95_us: None,
            legacy: LegacyCommand::Unavailable {
                reason: "legacy surface was an E2E matrix script, not a direct CLI parity wrapper",
            },
        },
    ]
}

fn run_case(
    case: GuardrailCase,
    am: &Path,
    binary_dir: &Path,
    repo: &Path,
    iterations: usize,
) -> GuardrailCaseResult {
    let native_budget_p95_us =
        read_surface_u64("PERF_GUARDRAIL_NATIVE_BUDGET_P95_US", case.surface_id)
            .unwrap_or(case.native_budget_p95_us);
    let max_native_delta_p95_us =
        read_surface_u64("PERF_GUARDRAIL_MAX_DELTA_P95_US", case.surface_id)
            .or(case.max_native_delta_p95_us);

    let native_command = format_command("am", case.native_args);
    let native_samples_result = run_command_samples(am, case.native_args, iterations, binary_dir);
    let (native_samples_us, native_p95_us, native_error) = match native_samples_result {
        Ok(samples) => {
            let p95 = samples_p95(&samples);
            (samples, p95, None)
        }
        Err(err) => (Vec::new(), None, Some(err)),
    };
    let native_budget_ok = native_p95_us.is_some_and(|p95| p95 <= native_budget_p95_us);

    let mut legacy_command = None;
    let mut legacy_status = String::from("unavailable");
    let mut legacy_unavailable_reason = None;
    let mut legacy_samples_us = None;
    let mut legacy_p95_us = None;
    let mut legacy_error = None;

    match case.legacy {
        LegacyCommand::Unavailable { reason } => {
            legacy_unavailable_reason = Some(reason.to_string());
        }
        LegacyCommand::Script {
            rel_path,
            args,
            unavailable_reason,
        } => {
            let legacy_path = repo.join(rel_path);
            legacy_command = Some(format_command(&legacy_path.display().to_string(), args));
            if !legacy_path.is_file() {
                legacy_unavailable_reason = Some(format!(
                    "{unavailable_reason}; missing path '{}'",
                    legacy_path.display()
                ));
            } else {
                match run_command_samples(&legacy_path, args, iterations, binary_dir) {
                    Ok(samples) => {
                        legacy_p95_us = samples_p95(&samples);
                        legacy_samples_us = Some(samples);
                        legacy_status = String::from("measured");
                    }
                    Err(err) => {
                        legacy_status = String::from("error");
                        legacy_error = Some(err);
                    }
                }
            }
        }
    }

    let delta_p95_us = match (native_p95_us, legacy_p95_us) {
        (Some(native), Some(legacy)) => Some(native as i64 - legacy as i64),
        _ => None,
    };
    let delta_ok = match (delta_p95_us, max_native_delta_p95_us) {
        (Some(delta), Some(max_delta)) => Some(delta <= max_delta as i64),
        (Some(_), None) => Some(true),
        _ => None,
    };

    let legacy_component_ok = legacy_error.is_none() && delta_ok.unwrap_or(true);
    let passed = native_budget_ok && native_error.is_none() && legacy_component_ok;

    GuardrailCaseResult {
        surface_id: case.surface_id.to_string(),
        native_command,
        legacy_command,
        iterations,
        native_samples_us,
        native_p95_us,
        native_budget_p95_us,
        native_budget_ok,
        native_error,
        legacy_status,
        legacy_unavailable_reason,
        legacy_samples_us,
        legacy_p95_us,
        legacy_error,
        delta_p95_us,
        max_native_delta_p95_us,
        delta_ok,
        passed,
    }
}

#[test]
fn perf_migration_guardrails() {
    let am = am_bin();
    let binary_dir = am.parent().expect("target dir").to_path_buf();
    let repo = repo_root();

    let iterations = read_u64_env("PERF_GUARDRAIL_ITERATIONS")
        .and_then(|raw| usize::try_from(raw).ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ITERATIONS);

    let cases = guardrail_cases()
        .iter()
        .copied()
        .map(|case| run_case(case, &am, &binary_dir, &repo, iterations))
        .collect::<Vec<_>>();

    let artifact = GuardrailSuiteArtifact {
        schema_version: 1,
        run_ts: chrono::Utc::now().to_rfc3339(),
        iterations,
        environment: environment_profile(),
        cases,
    };
    save_artifact(&artifact);

    for row in &artifact.cases {
        eprintln!(
            "guardrail {}: native_p95_us={:?} legacy_p95_us={:?} passed={} legacy_status={}",
            row.surface_id, row.native_p95_us, row.legacy_p95_us, row.passed, row.legacy_status
        );
    }

    let failures = artifact
        .cases
        .iter()
        .filter(|row| !row.passed)
        .map(|row| {
            format!(
                "- {} native_p95_us={:?} budget={} native_error={:?} legacy_status={} legacy_error={:?} delta_p95_us={:?} max_delta={:?}",
                row.surface_id,
                row.native_p95_us,
                row.native_budget_p95_us,
                row.native_error,
                row.legacy_status,
                row.legacy_error,
                row.delta_p95_us,
                row.max_native_delta_p95_us
            )
        })
        .collect::<Vec<_>>();

    assert!(
        failures.is_empty(),
        "perf migration guardrail failures:\n{}",
        failures.join("\n")
    );
}
