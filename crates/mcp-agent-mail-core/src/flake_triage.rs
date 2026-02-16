//! Flake triage harness and failure-forensics automation (br-3vwi.10.5).
//!
//! Provides infrastructure to capture, analyze, and reproduce intermittent
//! test failures. Integrates with the deterministic `test_harness` module
//! and produces structured artifacts for CI debugging.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use mcp_agent_mail_core::flake_triage::{FailureContext, FlakeReport};
//!
//! let ctx = FailureContext::capture("my_test", Some(42), "assertion failed: x == 3");
//! ctx.write_artifact(&artifact_dir)?;
//! ```
//!
//! # Shell reproduction
//!
//! ```bash
//! # From CI output or artifact:
//! HARNESS_SEED=42 cargo test --test my_suite -- my_test
//!
//! # Or use the native CLI:
//! am flake-triage tests/artifacts/flake_triage/20260210_*/failure_context.json
//! ```

#![allow(clippy::missing_const_for_fn)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::test_harness::ReproContext;

// ── Failure Context ──────────────────────────────────────────────────

/// Captures all information needed to diagnose and reproduce a test failure.
///
/// Serialized as `failure_context.json` in the test artifact directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureContext {
    /// Test name that failed.
    pub test_name: String,
    /// Harness seed (if deterministic harness was used).
    pub harness_seed: Option<u64>,
    /// E2E seed (if shell E2E harness was used).
    pub e2e_seed: Option<String>,
    /// Failure message or assertion text.
    pub failure_message: String,
    /// ISO-8601 timestamp of the failure.
    pub failure_ts: String,
    /// Reproduction command (copy-paste friendly).
    pub repro_command: String,
    /// Optional `ReproContext` from the deterministic harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro_context: Option<ReproContext>,
    /// Environment snapshot (secrets redacted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_snapshot: BTreeMap<String, String>,
    /// Resident set size at failure time (KB).
    pub rss_kb: u64,
    /// Process uptime at failure (seconds).
    pub uptime_secs: f64,
    /// Failure category (auto-classified).
    pub category: FailureCategory,
    /// Additional diagnostic notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Auto-classification of failure root cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Assertion failure (deterministic bug).
    Assertion,
    /// Timing-sensitive (p95 near budget, debounce, sleep-dependent).
    Timing,
    /// Resource contention (lock, pool exhaustion, circuit breaker).
    Contention,
    /// Nondeterministic (can't reproduce with same seed).
    Nondeterministic,
    /// CI-specific (resource limits, network, disk).
    CiEnvironment,
    /// Unknown classification.
    Unknown,
}

impl FailureContext {
    /// Capture a failure context from the current process state.
    #[must_use]
    pub fn capture(test_name: &str, harness_seed: Option<u64>, failure_message: &str) -> Self {
        let now = chrono::Utc::now();
        let env_snapshot = capture_env_snapshot();
        let category = classify_failure(failure_message, &env_snapshot);

        // Build repro command
        let mut repro_parts = Vec::new();
        if let Some(seed) = harness_seed {
            repro_parts.push(format!("HARNESS_SEED={seed}"));
        }
        if let Ok(e2e_seed) = std::env::var("E2E_SEED") {
            repro_parts.push(format!("E2E_SEED={e2e_seed}"));
        }
        repro_parts.push(format!("cargo test {test_name} -- --nocapture"));
        let repro_command = repro_parts.join(" ");

        let e2e_seed = std::env::var("E2E_SEED").ok();

        Self {
            test_name: test_name.to_string(),
            harness_seed,
            e2e_seed,
            failure_message: failure_message.to_string(),
            failure_ts: now.to_rfc3339(),
            repro_command,
            repro_context: None,
            env_snapshot,
            rss_kb: read_rss_kb(),
            uptime_secs: read_uptime_secs(),
            category,
            notes: Vec::new(),
        }
    }

    /// Attach a `ReproContext` from a deterministic harness.
    #[must_use]
    pub fn with_repro(mut self, repro: &ReproContext) -> Self {
        self.repro_context = Some(repro.clone());
        // Update repro command to use the full context
        self.repro_command = repro.repro_command();
        self
    }

    /// Add a diagnostic note.
    pub fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Write the failure context as a JSON artifact.
    ///
    /// # Errors
    /// Returns `Err` on serialization or I/O failure.
    pub fn write_artifact(&self, dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let path = dir.join("failure_context.json");
        std::fs::write(&path, &json)?;
        eprintln!("flake-triage artifact: {}", path.display());
        Ok(())
    }
}

// ── Flake Report ─────────────────────────────────────────────────────

/// A single test run outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    /// Run index (1-based).
    pub run: u32,
    /// Whether the test passed.
    pub passed: bool,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Failure message (if failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// Seed used for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// Aggregated flake report from multiple runs of the same test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeReport {
    /// Report generation timestamp.
    pub generated_at: String,
    /// Test name.
    pub test_name: String,
    /// Total runs attempted.
    pub total_runs: u32,
    /// Number of passes.
    pub passes: u32,
    /// Number of failures.
    pub failures: u32,
    /// Flake rate (failures / `total_runs`).
    pub flake_rate: f64,
    /// Individual run outcomes.
    pub runs: Vec<RunOutcome>,
    /// Failure message histogram (message → count).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub failure_histogram: BTreeMap<String, u32>,
    /// Seeds that produced failures (for targeted replay).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_seeds: Vec<u64>,
    /// Verdict: deterministic, flaky, or environment-dependent.
    pub verdict: FlakeVerdict,
    /// Suggested remediation.
    pub remediation: String,
}

/// Verdict from flake analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeVerdict {
    /// Test always passes — no flakiness detected.
    Stable,
    /// Test always fails — deterministic bug, not a flake.
    DeterministicFailure,
    /// Test intermittently fails — genuine flake.
    Flaky,
    /// Single run, can't determine flakiness.
    Inconclusive,
}

impl FlakeReport {
    /// Create a new report from a set of run outcomes.
    #[must_use]
    pub fn from_runs(test_name: &str, runs: Vec<RunOutcome>) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let total = runs.len() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let passes = runs.iter().filter(|r| r.passed).count() as u32;
        let failures = total - passes;
        let flake_rate = if total == 0 {
            0.0
        } else {
            f64::from(failures) / f64::from(total)
        };

        // Build histogram
        let mut histogram = BTreeMap::new();
        for run in &runs {
            if let Some(ref msg) = run.failure_message {
                // Normalize: take first line only
                let key = msg.lines().next().unwrap_or(msg).to_string();
                *histogram.entry(key).or_insert(0) += 1;
            }
        }

        // Collect failing seeds
        let failing_seeds: Vec<u64> = runs
            .iter()
            .filter(|r| !r.passed)
            .filter_map(|r| r.seed)
            .collect();

        let verdict = if total <= 1 {
            FlakeVerdict::Inconclusive
        } else if failures == 0 {
            FlakeVerdict::Stable
        } else if passes == 0 {
            FlakeVerdict::DeterministicFailure
        } else {
            FlakeVerdict::Flaky
        };

        let remediation = match verdict {
            FlakeVerdict::Stable => "No action needed.".to_string(),
            FlakeVerdict::DeterministicFailure => {
                let seed_hint = failing_seeds
                    .first()
                    .map_or(String::new(), |s| format!(" (try: HARNESS_SEED={s})"));
                format!("Fix the test — fails on every run.{seed_hint}")
            }
            FlakeVerdict::Flaky => {
                let rate_pct = flake_rate * 100.0;
                let top_msg = histogram
                    .iter()
                    .max_by_key(|(_, c)| *c)
                    .map_or("(unknown)", |(m, _)| m.as_str());
                format!(
                    "Flake rate: {rate_pct:.1}%. Most common failure: {top_msg}. \
                     Replay failing seeds: {:?}",
                    &failing_seeds[..failing_seeds.len().min(5)]
                )
            }
            FlakeVerdict::Inconclusive => "Run more iterations to determine stability.".to_string(),
        };

        Self {
            generated_at: chrono::Utc::now().to_rfc3339(),
            test_name: test_name.to_string(),
            total_runs: total,
            passes,
            failures,
            flake_rate,
            runs,
            failure_histogram: histogram,
            failing_seeds,
            verdict,
            remediation,
        }
    }

    /// Write the report as a JSON artifact.
    ///
    /// # Errors
    /// Returns `Err` on serialization or I/O failure.
    pub fn write_artifact(&self, dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let path = dir.join("flake_report.json");
        std::fs::write(&path, &json)?;
        eprintln!("flake-triage report: {}", path.display());
        Ok(())
    }
}

// ── Multi-Seed Runner ────────────────────────────────────────────────

/// Run a test closure with multiple seeds and collect outcomes.
///
/// The closure receives a seed and returns `Ok(())` on pass or
/// `Err(message)` on failure.
///
/// ```rust,ignore
/// let report = run_with_seeds("my_test", &[1, 2, 3, 42, 100], |seed| {
///     let h = Harness::with_seed(seed, "my_test");
///     // ... test logic ...
///     Ok(())
/// });
/// assert_eq!(report.verdict, FlakeVerdict::Stable);
/// ```
pub fn run_with_seeds<F>(test_name: &str, seeds: &[u64], test_fn: F) -> FlakeReport
where
    F: Fn(u64) -> Result<(), String>,
{
    let mut runs = Vec::with_capacity(seeds.len());
    for (i, &seed) in seeds.iter().enumerate() {
        let start = std::time::Instant::now();
        let result = test_fn(seed);
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = start.elapsed().as_millis() as u64;

        runs.push(RunOutcome {
            #[allow(clippy::cast_possible_truncation)]
            run: (i + 1) as u32,
            passed: result.is_ok(),
            duration_ms,
            failure_message: result.err(),
            seed: Some(seed),
        });
    }
    FlakeReport::from_runs(test_name, runs)
}

/// Default seed corpus for flake detection.
///
/// Includes edge-case seeds (0, 1, max) plus a spread of values to catch
/// nondeterminism across the PRNG state space.
pub const DEFAULT_FLAKE_SEEDS: &[u64] = &[
    0,
    1,
    2,
    42,
    100,
    255,
    1000,
    12345,
    65535,
    999_999,
    0xDEAD_BEEF,
    0xCAFE_BABE,
    0x1234_5678,
    0xFFFF_FFFF,
    u64::MAX,
    u64::MAX / 2,
    u64::MAX / 3,
];

// ── Failure Classification ───────────────────────────────────────────

/// Classify a failure message into a [`FailureCategory`].
#[must_use]
pub fn classify_failure(message: &str, env: &BTreeMap<String, String>) -> FailureCategory {
    let lower = message.to_ascii_lowercase();

    // Timing patterns
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("took too long")
        || lower.contains("deadline exceeded")
        || lower.contains("budget")
        || lower.contains("p95")
        || lower.contains("latency")
    {
        return FailureCategory::Timing;
    }

    // Contention patterns
    if lower.contains("lock")
        || lower.contains("busy")
        || lower.contains("pool exhausted")
        || lower.contains("circuit breaker")
        || lower.contains("database is locked")
        || lower.contains("disk i/o error")
        || lower.contains("too many open files")
    {
        return FailureCategory::Contention;
    }

    // CI environment patterns
    if lower.contains("address already in use")
        || lower.contains("connection refused")
        || lower.contains("no such file")
        || lower.contains("permission denied")
        || lower.contains("out of memory")
    {
        return FailureCategory::CiEnvironment;
    }

    // Check for CI environment indicators
    if (env.contains_key("CI") || env.contains_key("GITHUB_ACTIONS"))
        && (lower.contains("killed") || lower.contains("signal"))
    {
        return FailureCategory::CiEnvironment;
    }

    // Standard assertions
    if lower.contains("assertion")
        || lower.contains("assert_eq")
        || lower.contains("assert_ne")
        || lower.contains("panic")
        || lower.contains("expected")
    {
        return FailureCategory::Assertion;
    }

    FailureCategory::Unknown
}

// ── Environment Capture ──────────────────────────────────────────────

/// Capture relevant environment variables, redacting secrets.
#[must_use]
pub fn capture_env_snapshot() -> BTreeMap<String, String> {
    let relevant_prefixes = [
        "HARNESS_",
        "E2E_",
        "SOAK_",
        "MCP_AGENT_MAIL_",
        "CIRCUIT_",
        "RUST_",
        "CARGO_",
        "CI",
        "GITHUB_",
        "AM_",
        "WORKTREES_",
    ];
    let secret_patterns = [
        "KEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "CREDENTIAL",
        "AUTH",
        "API_KEY",
    ];

    let mut snapshot = BTreeMap::new();
    for (key, value) in std::env::vars() {
        let dominated = relevant_prefixes.iter().any(|p| key.starts_with(p));
        if !dominated {
            continue;
        }
        let is_secret = secret_patterns
            .iter()
            .any(|p| key.to_ascii_uppercase().contains(p));
        let display_value = if is_secret {
            "[REDACTED]".to_string()
        } else {
            value
        };
        snapshot.insert(key, display_value);
    }
    snapshot
}

// ── System Info Helpers ──────────────────────────────────────────────

/// Read resident set size from `/proc/self/statm` (Linux).
/// Returns 0 on non-Linux or on read failure.
#[must_use]
pub fn read_rss_kb() -> u64 {
    crate::memory::read_rss_bytes().map_or(0, |b| b / 1024)
}

/// Read process uptime by checking `/proc/self/stat` start time.
/// Returns 0.0 on failure.
#[must_use]
pub fn read_uptime_secs() -> f64 {
    crate::diagnostics::process_uptime().as_secs_f64()
}

// ── Artifact Scanning (br-36xx) ─────────────────────────────────────

/// A scanned failure artifact with file location and parsed context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedArtifact {
    /// Absolute path to the artifact file.
    pub path: std::path::PathBuf,
    /// Parsed failure context (or None if the file was malformed).
    pub context: FailureContext,
}

/// Read a single failure context from a JSON artifact file.
///
/// # Errors
/// Returns `Err` on I/O or deserialization failure.
pub fn read_artifact(path: &Path) -> std::io::Result<FailureContext> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(std::io::Error::other)
}

/// Scan a directory tree for `failure_context.json` artifacts.
///
/// Walks `dir` recursively, finding files named `failure_context.json`.
/// Malformed files are silently skipped (logged to stderr).
/// Results are sorted by failure timestamp (most recent first).
#[must_use]
pub fn scan_artifacts(dir: &Path) -> Vec<ScannedArtifact> {
    let mut results = Vec::new();
    scan_dir_recursive(dir, &mut results);
    // Sort by timestamp (most recent first)
    results.sort_by(|a, b| b.context.failure_ts.cmp(&a.context.failure_ts));
    results
}

fn scan_dir_recursive(dir: &Path, results: &mut Vec<ScannedArtifact>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, results);
        } else if path
            .file_name()
            .is_some_and(|n| n == "failure_context.json")
        {
            match read_artifact(&path) {
                Ok(ctx) => results.push(ScannedArtifact {
                    path: path.clone(),
                    context: ctx,
                }),
                Err(e) => {
                    eprintln!(
                        "flake-triage: skipping malformed artifact {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
}

// ── Failure Reproduction (br-1kk7) ─────────────────────────────────

/// Configuration for reproducing a failure from an artifact.
#[derive(Debug, Clone)]
pub struct ReproductionConfig {
    /// Path to the `failure_context.json` artifact.
    pub artifact_path: std::path::PathBuf,
    /// Pass `--nocapture` to cargo test for verbose output.
    pub verbose: bool,
    /// Timeout per test run.
    pub timeout: std::time::Duration,
}

/// Result of a reproduction attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReproductionResult {
    /// Test name reproduced.
    pub test_name: String,
    /// Seed used (if any).
    pub seed: Option<u64>,
    /// Whether the failure was reproduced (test failed again).
    pub reproduced: bool,
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Elapsed wall-clock time.
    pub elapsed_ms: u64,
}

/// Reproduce a failure from an artifact file.
///
/// Reads the artifact, reconstructs the repro command, and executes it
/// as a subprocess. Returns the result including whether the failure
/// was successfully reproduced.
///
/// # Errors
/// Returns `Err` on I/O failure or if the artifact is malformed.
pub fn reproduce_failure(config: &ReproductionConfig) -> std::io::Result<ReproductionResult> {
    let ctx = read_artifact(&config.artifact_path)?;

    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("test");

    // Add package flags for the standard test packages
    for pkg in &[
        "mcp-agent-mail-core",
        "mcp-agent-mail-server",
        "mcp-agent-mail-db",
    ] {
        cmd.arg("-p").arg(pkg);
    }

    cmd.arg(&ctx.test_name).arg("--");
    if config.verbose {
        cmd.arg("--nocapture");
    }

    // Set seed environment variables
    if let Some(seed) = ctx.harness_seed {
        cmd.env("HARNESS_SEED", seed.to_string());
    }
    if let Some(ref e2e_seed) = ctx.e2e_seed {
        cmd.env("E2E_SEED", e2e_seed);
    }

    // Replay captured environment (minus secrets)
    for (key, value) in &ctx.env_snapshot {
        if value != "[REDACTED]" {
            cmd.env(key, value);
        }
    }

    let start = std::time::Instant::now();
    let output = cmd.output()?;
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let exit_code = output.status.code().unwrap_or(-1);

    Ok(ReproductionResult {
        test_name: ctx.test_name,
        seed: ctx.harness_seed,
        reproduced: !output.status.success(),
        exit_code,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        elapsed_ms,
    })
}

// ── Multi-Seed Subprocess Runner (br-154k) ─────────────────────────

/// Configuration for subprocess-based multi-seed flake detection.
#[derive(Debug, Clone)]
pub struct MultiSeedConfig {
    /// Test name to run.
    pub test_name: String,
    /// Number of seeds to test.
    pub num_seeds: usize,
    /// Extra cargo packages to include (default: core, server, db).
    pub packages: Vec<String>,
    /// Timeout per individual test run.
    pub timeout: std::time::Duration,
}

impl Default for MultiSeedConfig {
    fn default() -> Self {
        Self {
            test_name: String::new(),
            num_seeds: DEFAULT_FLAKE_SEEDS.len(),
            packages: vec![
                "mcp-agent-mail-core".to_string(),
                "mcp-agent-mail-server".to_string(),
                "mcp-agent-mail-db".to_string(),
            ],
            timeout: std::time::Duration::from_secs(60),
        }
    }
}

/// Extend a seed corpus with pseudo-random values to reach `target_count`.
///
/// Uses a simple deterministic PRNG (splitmix64) seeded from the corpus
/// to generate additional seeds reproducibly.
#[must_use]
pub fn extend_seeds(base: &[u64], target_count: usize) -> Vec<u64> {
    let mut seeds: Vec<u64> = base.to_vec();
    if seeds.len() >= target_count {
        seeds.truncate(target_count);
        return seeds;
    }

    // Simple splitmix64 for deterministic extension
    let mut state: u64 = base
        .iter()
        .copied()
        .fold(0x9E37_79B9_7F4A_7C15, u64::wrapping_add);
    while seeds.len() < target_count {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        if !seeds.contains(&z) {
            seeds.push(z);
        }
    }
    seeds
}

/// Run a test as a subprocess with multiple seeds, collecting outcomes.
///
/// Unlike [`run_with_seeds`] which uses closures (in-process), this spawns
/// `cargo test` as a subprocess for each seed. Suitable for CLI integration
/// where full binary startup and process isolation is needed.
#[must_use]
pub fn run_multi_seed_subprocess(config: &MultiSeedConfig) -> FlakeReport {
    let seeds = extend_seeds(DEFAULT_FLAKE_SEEDS, config.num_seeds);
    let mut runs = Vec::with_capacity(seeds.len());

    for (i, &seed) in seeds.iter().enumerate() {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("test");

        for pkg in &config.packages {
            cmd.arg("-p").arg(pkg);
        }

        cmd.arg(&config.test_name)
            .arg("--")
            .arg("--nocapture")
            .env("HARNESS_SEED", seed.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let start = std::time::Instant::now();
        let status = cmd.status();
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = start.elapsed().as_millis() as u64;

        let (passed, exit_code) = status
            .as_ref()
            .map_or((false, -1), |s| (s.success(), s.code().unwrap_or(-1)));

        runs.push(RunOutcome {
            #[allow(clippy::cast_possible_truncation)]
            run: (i + 1) as u32,
            passed,
            duration_ms,
            failure_message: if passed {
                None
            } else {
                Some(format!("exit {exit_code}"))
            },
            seed: Some(seed),
        });
    }

    FlakeReport::from_runs(&config.test_name, runs)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failure_context() {
        let ctx = FailureContext::capture("test_example", Some(42), "assertion failed: x == 3");
        assert_eq!(ctx.test_name, "test_example");
        assert_eq!(ctx.harness_seed, Some(42));
        assert!(!ctx.failure_ts.is_empty());
        assert!(ctx.repro_command.contains("HARNESS_SEED=42"));
        assert!(ctx.repro_command.contains("test_example"));
    }

    #[test]
    fn capture_without_seed() {
        let ctx = FailureContext::capture("test_no_seed", None, "oops");
        assert!(ctx.harness_seed.is_none());
        assert!(ctx.repro_command.contains("test_no_seed"));
    }

    #[test]
    fn failure_context_with_repro() {
        let repro = ReproContext {
            seed: 99,
            clock_base_micros: 1_704_067_200_000_000,
            clock_step_micros: 1_000_000,
            id_base: 1,
            test_name: "repro_test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            target: "x86_64".to_string(),
            extra: vec![("SOAK_DURATION_SECS".to_string(), "30".to_string())],
        };
        let ctx = FailureContext::capture("repro_test", Some(99), "fail").with_repro(&repro);
        assert!(ctx.repro_command.contains("HARNESS_SEED=99"));
        assert!(ctx.repro_context.is_some());
    }

    #[test]
    fn failure_context_add_note() {
        let mut ctx = FailureContext::capture("test_notes", None, "fail");
        ctx.add_note("Circuit breaker was open for DB");
        ctx.add_note("RSS was 450MB at failure time");
        assert_eq!(ctx.notes.len(), 2);
    }

    #[test]
    fn failure_context_serialization_roundtrip() {
        let ctx = FailureContext::capture("test_serde", Some(42), "assert failed");
        let json = serde_json::to_string_pretty(&ctx).unwrap();
        let restored: FailureContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.test_name, "test_serde");
        assert_eq!(restored.harness_seed, Some(42));
    }

    #[test]
    fn failure_context_write_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = FailureContext::capture("test_write", Some(1), "fail");
        ctx.write_artifact(dir.path()).unwrap();
        let path = dir.path().join("failure_context.json");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("test_write"));
    }

    // ── Classification Tests ─────────────────────────────────────────

    #[test]
    fn classify_timing() {
        let env = BTreeMap::new();
        assert_eq!(
            classify_failure("test took too long: 5.2s", &env),
            FailureCategory::Timing
        );
        assert_eq!(
            classify_failure("p95 latency exceeded budget", &env),
            FailureCategory::Timing
        );
        assert_eq!(
            classify_failure("timeout waiting for response", &env),
            FailureCategory::Timing
        );
    }

    #[test]
    fn classify_contention() {
        let env = BTreeMap::new();
        assert_eq!(
            classify_failure("database is locked", &env),
            FailureCategory::Contention
        );
        assert_eq!(
            classify_failure("pool exhausted: 0 connections available", &env),
            FailureCategory::Contention
        );
        assert_eq!(
            classify_failure("circuit breaker open for DB subsystem", &env),
            FailureCategory::Contention
        );
    }

    #[test]
    fn classify_ci_environment() {
        let env = BTreeMap::new();
        assert_eq!(
            classify_failure("address already in use: 127.0.0.1:8080", &env),
            FailureCategory::CiEnvironment
        );
        assert_eq!(
            classify_failure("permission denied: /tmp/test.db", &env),
            FailureCategory::CiEnvironment
        );
    }

    #[test]
    fn classify_assertion() {
        let env = BTreeMap::new();
        assert_eq!(
            classify_failure("assertion failed: left == right", &env),
            FailureCategory::Assertion
        );
        assert_eq!(
            classify_failure("panic at tests/foo.rs:42", &env),
            FailureCategory::Assertion
        );
    }

    #[test]
    fn classify_unknown() {
        let env = BTreeMap::new();
        assert_eq!(
            classify_failure("something weird happened", &env),
            FailureCategory::Unknown
        );
    }

    #[test]
    fn classify_ci_killed_signal() {
        let mut env = BTreeMap::new();
        env.insert("CI".to_string(), "true".to_string());
        assert_eq!(
            classify_failure("process killed by signal 9", &env),
            FailureCategory::CiEnvironment
        );
    }

    // ── Flake Report Tests ───────────────────────────────────────────

    #[test]
    fn flake_report_stable() {
        let runs = vec![
            RunOutcome {
                run: 1,
                passed: true,
                duration_ms: 10,
                failure_message: None,
                seed: Some(1),
            },
            RunOutcome {
                run: 2,
                passed: true,
                duration_ms: 12,
                failure_message: None,
                seed: Some(2),
            },
            RunOutcome {
                run: 3,
                passed: true,
                duration_ms: 11,
                failure_message: None,
                seed: Some(3),
            },
        ];
        let report = FlakeReport::from_runs("stable_test", runs);
        assert_eq!(report.verdict, FlakeVerdict::Stable);
        assert_eq!(report.passes, 3);
        assert_eq!(report.failures, 0);
        assert!((report.flake_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn flake_report_deterministic_failure() {
        let runs = vec![
            RunOutcome {
                run: 1,
                passed: false,
                duration_ms: 5,
                failure_message: Some("bug".to_string()),
                seed: Some(1),
            },
            RunOutcome {
                run: 2,
                passed: false,
                duration_ms: 6,
                failure_message: Some("bug".to_string()),
                seed: Some(2),
            },
        ];
        let report = FlakeReport::from_runs("always_fails", runs);
        assert_eq!(report.verdict, FlakeVerdict::DeterministicFailure);
        assert_eq!(report.failures, 2);
        assert!(!report.failing_seeds.is_empty());
    }

    #[test]
    fn flake_report_flaky() {
        let runs = vec![
            RunOutcome {
                run: 1,
                passed: true,
                duration_ms: 10,
                failure_message: None,
                seed: Some(1),
            },
            RunOutcome {
                run: 2,
                passed: false,
                duration_ms: 15,
                failure_message: Some("timeout".to_string()),
                seed: Some(2),
            },
            RunOutcome {
                run: 3,
                passed: true,
                duration_ms: 11,
                failure_message: None,
                seed: Some(3),
            },
            RunOutcome {
                run: 4,
                passed: false,
                duration_ms: 20,
                failure_message: Some("timeout".to_string()),
                seed: Some(4),
            },
        ];
        let report = FlakeReport::from_runs("flaky_test", runs);
        assert_eq!(report.verdict, FlakeVerdict::Flaky);
        assert_eq!(report.passes, 2);
        assert_eq!(report.failures, 2);
        assert!((report.flake_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(report.failure_histogram["timeout"], 2);
        assert_eq!(report.failing_seeds, vec![2, 4]);
    }

    #[test]
    fn flake_report_inconclusive() {
        let runs = vec![RunOutcome {
            run: 1,
            passed: true,
            duration_ms: 10,
            failure_message: None,
            seed: Some(1),
        }];
        let report = FlakeReport::from_runs("single_run", runs);
        assert_eq!(report.verdict, FlakeVerdict::Inconclusive);
    }

    #[test]
    fn flake_report_empty() {
        let report = FlakeReport::from_runs("empty", vec![]);
        assert_eq!(report.verdict, FlakeVerdict::Inconclusive);
        assert_eq!(report.total_runs, 0);
    }

    #[test]
    fn flake_report_serialization() {
        let runs = vec![
            RunOutcome {
                run: 1,
                passed: true,
                duration_ms: 10,
                failure_message: None,
                seed: Some(42),
            },
            RunOutcome {
                run: 2,
                passed: false,
                duration_ms: 15,
                failure_message: Some("oops".to_string()),
                seed: Some(43),
            },
        ];
        let report = FlakeReport::from_runs("serde_test", runs);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let restored: FlakeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.test_name, "serde_test");
        assert_eq!(restored.verdict, FlakeVerdict::Flaky);
    }

    #[test]
    fn flake_report_write_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let report = FlakeReport::from_runs(
            "write_test",
            vec![RunOutcome {
                run: 1,
                passed: true,
                duration_ms: 5,
                failure_message: None,
                seed: None,
            }],
        );
        report.write_artifact(dir.path()).unwrap();
        let path = dir.path().join("flake_report.json");
        assert!(path.exists());
    }

    // ── Multi-Seed Runner Tests ──────────────────────────────────────

    #[test]
    fn run_with_seeds_all_pass() {
        let report = run_with_seeds("seed_test_pass", &[1, 2, 3, 4, 5], |_seed| Ok(()));
        assert_eq!(report.verdict, FlakeVerdict::Stable);
        assert_eq!(report.total_runs, 5);
    }

    #[test]
    fn run_with_seeds_some_fail() {
        let report = run_with_seeds("seed_test_flaky", &[1, 2, 3, 4, 5], |seed| {
            if seed % 2 == 0 {
                Err("even seed fails".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(report.verdict, FlakeVerdict::Flaky);
        assert_eq!(report.failures, 2);
        assert_eq!(report.passes, 3);
        assert_eq!(report.failing_seeds, vec![2, 4]);
    }

    #[test]
    fn run_with_seeds_all_fail() {
        let report = run_with_seeds("seed_test_fail", &[1, 2, 3], |_| {
            Err("always fails".to_string())
        });
        assert_eq!(report.verdict, FlakeVerdict::DeterministicFailure);
    }

    #[test]
    fn default_seeds_not_empty() {
        assert!(DEFAULT_FLAKE_SEEDS.len() >= 10);
    }

    // ── Environment Capture Tests ────────────────────────────────────

    #[test]
    fn env_snapshot_captures_relevant_vars() {
        // Note: can't set env vars safely, but we can verify the function runs
        let snapshot = capture_env_snapshot();
        // Should capture CARGO_ prefixed vars at minimum
        assert!(
            snapshot.keys().any(|k| k.starts_with("CARGO_")),
            "Expected at least one CARGO_ var, got: {:?}",
            snapshot.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn rss_kb_non_negative() {
        let rss = read_rss_kb();
        // Should be > 0 on Linux
        assert!(
            rss > 0 || !cfg!(target_os = "linux"),
            "RSS should be positive on Linux"
        );
    }

    #[test]
    fn uptime_secs_non_negative() {
        let uptime = read_uptime_secs();
        assert!(uptime >= 0.0);
    }

    // ── Histogram / Remediation Tests ────────────────────────────────

    #[test]
    fn flake_report_histogram_multiple_messages() {
        let runs = vec![
            RunOutcome {
                run: 1,
                passed: false,
                duration_ms: 5,
                failure_message: Some("timeout".to_string()),
                seed: None,
            },
            RunOutcome {
                run: 2,
                passed: false,
                duration_ms: 6,
                failure_message: Some("timeout".to_string()),
                seed: None,
            },
            RunOutcome {
                run: 3,
                passed: false,
                duration_ms: 7,
                failure_message: Some("lock error".to_string()),
                seed: None,
            },
            RunOutcome {
                run: 4,
                passed: true,
                duration_ms: 8,
                failure_message: None,
                seed: None,
            },
        ];
        let report = FlakeReport::from_runs("hist_test", runs);
        assert_eq!(report.failure_histogram["timeout"], 2);
        assert_eq!(report.failure_histogram["lock error"], 1);
        assert!(report.remediation.contains("timeout"));
    }

    #[test]
    fn remediation_text_varies_by_verdict() {
        let stable = FlakeReport::from_runs(
            "s",
            vec![
                RunOutcome {
                    run: 1,
                    passed: true,
                    duration_ms: 1,
                    failure_message: None,
                    seed: None,
                },
                RunOutcome {
                    run: 2,
                    passed: true,
                    duration_ms: 1,
                    failure_message: None,
                    seed: None,
                },
            ],
        );
        assert!(stable.remediation.contains("No action"));

        let det_fail = FlakeReport::from_runs(
            "f",
            vec![
                RunOutcome {
                    run: 1,
                    passed: false,
                    duration_ms: 1,
                    failure_message: Some("x".to_string()),
                    seed: Some(42),
                },
                RunOutcome {
                    run: 2,
                    passed: false,
                    duration_ms: 1,
                    failure_message: Some("x".to_string()),
                    seed: Some(43),
                },
            ],
        );
        assert!(det_fail.remediation.contains("Fix the test"));
    }

    // ── Artifact Scanning Tests (br-36xx) ───────────────────────────

    #[test]
    fn read_artifact_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = FailureContext::capture("test_roundtrip", Some(42), "assertion failed");
        ctx.write_artifact(dir.path()).unwrap();

        let restored = read_artifact(&dir.path().join("failure_context.json")).unwrap();
        assert_eq!(restored.test_name, "test_roundtrip");
        assert_eq!(restored.harness_seed, Some(42));
        assert_eq!(restored.category, FailureCategory::Assertion);
    }

    #[test]
    fn read_artifact_missing_file() {
        let result = read_artifact(Path::new("/nonexistent/path/failure_context.json"));
        assert!(result.is_err());
    }

    #[test]
    fn read_artifact_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failure_context.json");
        std::fs::write(&path, "{ not valid json }").unwrap();
        let result = read_artifact(&path);
        assert!(result.is_err());
    }

    #[test]
    fn scan_artifacts_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let results = scan_artifacts(dir.path());
        assert!(results.is_empty());
    }

    #[test]
    fn scan_artifacts_finds_nested() {
        let dir = tempfile::tempdir().unwrap();

        // Create nested directories with artifacts
        let sub1 = dir.path().join("run1");
        let sub2 = dir.path().join("run2");
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::create_dir_all(&sub2).unwrap();

        let ctx1 = FailureContext::capture("test_a", Some(1), "timeout");
        ctx1.write_artifact(&sub1).unwrap();

        let ctx2 = FailureContext::capture("test_b", Some(2), "database is locked");
        ctx2.write_artifact(&sub2).unwrap();

        let results = scan_artifacts(dir.path());
        assert_eq!(results.len(), 2);

        let names: Vec<&str> = results
            .iter()
            .map(|r| r.context.test_name.as_str())
            .collect();
        assert!(names.contains(&"test_a"));
        assert!(names.contains(&"test_b"));
    }

    #[test]
    fn scan_artifacts_skips_malformed() {
        let dir = tempfile::tempdir().unwrap();

        // Valid artifact
        let sub1 = dir.path().join("valid");
        std::fs::create_dir_all(&sub1).unwrap();
        let ctx = FailureContext::capture("good_test", Some(1), "fail");
        ctx.write_artifact(&sub1).unwrap();

        // Malformed artifact
        let sub2 = dir.path().join("bad");
        std::fs::create_dir_all(&sub2).unwrap();
        std::fs::write(sub2.join("failure_context.json"), "not json").unwrap();

        let results = scan_artifacts(dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].context.test_name, "good_test");
    }

    #[test]
    fn scan_artifacts_nonexistent_dir() {
        let results = scan_artifacts(Path::new("/nonexistent/dir"));
        assert!(results.is_empty());
    }

    #[test]
    fn scanned_artifact_has_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = FailureContext::capture("path_test", None, "fail");
        ctx.write_artifact(dir.path()).unwrap();

        let results = scan_artifacts(dir.path());
        assert_eq!(results.len(), 1);
        assert!(results[0].path.ends_with("failure_context.json"));
    }

    // ── Seed Extension Tests (br-154k) ──────────────────────────────

    #[test]
    fn extend_seeds_noop_when_enough() {
        let base = &[1, 2, 3, 4, 5];
        let extended = extend_seeds(base, 3);
        assert_eq!(extended.len(), 3);
        assert_eq!(extended, vec![1, 2, 3]);
    }

    #[test]
    fn extend_seeds_adds_values() {
        let base = &[1, 2, 3];
        let extended = extend_seeds(base, 10);
        assert_eq!(extended.len(), 10);
        // Original seeds preserved
        assert_eq!(&extended[..3], &[1, 2, 3]);
        // No duplicates
        let mut uniq = extended.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), extended.len());
    }

    #[test]
    fn extend_seeds_deterministic() {
        let base = &[42, 100, 255];
        let run1 = extend_seeds(base, 20);
        let run2 = extend_seeds(base, 20);
        assert_eq!(run1, run2);
    }

    #[test]
    fn extend_seeds_from_default_corpus() {
        let extended = extend_seeds(DEFAULT_FLAKE_SEEDS, 30);
        assert_eq!(extended.len(), 30);
        assert_eq!(&extended[..DEFAULT_FLAKE_SEEDS.len()], DEFAULT_FLAKE_SEEDS);
    }

    #[test]
    fn extend_seeds_exact_count() {
        let extended = extend_seeds(DEFAULT_FLAKE_SEEDS, DEFAULT_FLAKE_SEEDS.len());
        assert_eq!(extended.len(), DEFAULT_FLAKE_SEEDS.len());
        assert_eq!(extended, DEFAULT_FLAKE_SEEDS);
    }

    // ── Reproduction Config Tests (br-1kk7) ────────────────────────

    #[test]
    fn reproduction_result_serialization() {
        let result = ReproductionResult {
            test_name: "repro_test".to_string(),
            seed: Some(42),
            reproduced: true,
            exit_code: 101,
            stdout: "test output".to_string(),
            stderr: "error output".to_string(),
            elapsed_ms: 1234,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ReproductionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.test_name, "repro_test");
        assert_eq!(restored.seed, Some(42));
        assert!(restored.reproduced);
        assert_eq!(restored.exit_code, 101);
    }

    #[test]
    fn multi_seed_config_default() {
        let config = MultiSeedConfig::default();
        assert_eq!(config.num_seeds, DEFAULT_FLAKE_SEEDS.len());
        assert_eq!(config.packages.len(), 3);
        assert_eq!(config.timeout, std::time::Duration::from_secs(60));
    }

    #[test]
    fn scanned_artifact_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = FailureContext::capture("serde_scan", Some(99), "panic");
        ctx.write_artifact(dir.path()).unwrap();

        let results = scan_artifacts(dir.path());
        assert_eq!(results.len(), 1);
        let json = serde_json::to_string_pretty(&results[0]).unwrap();
        let restored: ScannedArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context.test_name, "serde_scan");
    }
}
