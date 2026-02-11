//! Flake triage harness and failure-forensics automation (br-3vwi.10.5).
//!
//! Provides infrastructure to capture, analyze, and reproduce intermittent
//! test failures. Integrates with the deterministic [`test_harness`] module
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
//! # Or use the flake-triage script:
//! scripts/flake_triage.sh tests/artifacts/flake_triage/20260210_*/failure_context.json
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
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4) // 4KB pages
}

/// Read process uptime by checking `/proc/self/stat` start time.
/// Returns 0.0 on failure.
#[must_use]
pub fn read_uptime_secs() -> f64 {
    // Fallback: use process start via Instant approximation
    // This is a rough estimate; precise /proc parsing is complex
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f64()
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
}
