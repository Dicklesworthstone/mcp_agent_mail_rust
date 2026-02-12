//! CI gate configuration data model for `am ci` command.
//!
//! This module defines the canonical data contracts for the native CI command path,
//! replacing the implicit schema encoded in `scripts/ci.sh`.
//!
//! Schema version: `am_ci_gate_report.v1`

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// Enums
// ──────────────────────────────────────────────────────────────────────────────

/// Category of a CI gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateCategory {
    /// Code quality gates (format, lint, build, tests).
    Quality,
    /// Performance regression gates.
    Performance,
    /// Security and privacy gates.
    Security,
    /// Documentation gates.
    Docs,
}

impl GateCategory {
    /// Returns the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Performance => "performance",
            Self::Security => "security",
            Self::Docs => "docs",
        }
    }
}

impl std::fmt::Display for GateCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Status of a gate execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    /// Gate passed successfully.
    Pass,
    /// Gate failed.
    Fail,
    /// Gate was skipped (e.g., in quick mode).
    Skip,
}

impl GateStatus {
    /// Returns the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

impl std::fmt::Display for GateStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Release decision after running all gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decision {
    /// All required gates passed; safe to release.
    Go,
    /// One or more gates failed or were skipped; not safe to release.
    NoGo,
}

impl Decision {
    /// Returns the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::NoGo => "no-go",
        }
    }
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// CI run mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Full CI run including all gates.
    Full,
    /// Quick run skipping long-running E2E gates.
    Quick,
}

impl RunMode {
    /// Returns the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Quick => "quick",
        }
    }
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Gate Configuration
// ──────────────────────────────────────────────────────────────────────────────

/// A single CI gate definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    /// Human-readable gate name (e.g., "Format check").
    pub name: String,
    /// Gate category (quality, performance, security, docs).
    pub category: GateCategory,
    /// Command to execute (shell or cargo command parts).
    pub command: Vec<String>,
    /// If true, skip this gate in quick mode.
    pub skip_in_quick: bool,
    /// Optional parallel group: gates in same group can run concurrently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_group: Option<String>,
}

impl GateConfig {
    /// Creates a new gate config with common defaults.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        category: GateCategory,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            category,
            command: command.into_iter().map(Into::into).collect(),
            skip_in_quick: false,
            parallel_group: None,
        }
    }

    /// Builder: mark this gate as skippable in quick mode.
    #[must_use]
    pub fn skip_in_quick(mut self) -> Self {
        self.skip_in_quick = true;
        self
    }

    /// Builder: assign a parallel execution group.
    #[must_use]
    pub fn parallel_group(mut self, group: impl Into<String>) -> Self {
        self.parallel_group = Some(group.into());
        self
    }

    /// Returns the command as a display string.
    #[must_use]
    pub fn command_display(&self) -> String {
        self.command.join(" ")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Gate Result
// ──────────────────────────────────────────────────────────────────────────────

/// Result of running a single gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    /// Gate name (matches `GateConfig::name`).
    pub name: String,
    /// Gate category.
    pub category: GateCategory,
    /// Execution status.
    pub status: GateStatus,
    /// Elapsed execution time in seconds.
    pub elapsed_seconds: u64,
    /// Command that was executed (display string).
    pub command: String,
    /// Last N lines of stderr on failure (for diagnostics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

impl GateResult {
    /// Creates a pass result.
    #[must_use]
    pub fn pass(config: &GateConfig, elapsed: Duration) -> Self {
        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Pass,
            elapsed_seconds: elapsed.as_secs(),
            command: config.command_display(),
            stderr_tail: None,
        }
    }

    /// Creates a fail result with optional stderr tail.
    #[must_use]
    pub fn fail(config: &GateConfig, elapsed: Duration, stderr_tail: Option<String>) -> Self {
        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Fail,
            elapsed_seconds: elapsed.as_secs(),
            command: config.command_display(),
            stderr_tail,
        }
    }

    /// Creates a skip result.
    #[must_use]
    pub fn skip(config: &GateConfig, reason: impl Into<String>) -> Self {
        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Skip,
            elapsed_seconds: 0,
            command: reason.into(),
            stderr_tail: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Report Components
// ──────────────────────────────────────────────────────────────────────────────

/// Summary counts for gate execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GateSummary {
    /// Total number of gates.
    pub total: usize,
    /// Number of gates that passed.
    pub pass: usize,
    /// Number of gates that failed.
    pub fail: usize,
    /// Number of gates that were skipped.
    pub skip: usize,
}

impl GateSummary {
    /// Computes summary from a list of gate results.
    #[must_use]
    pub fn from_results(results: &[GateResult]) -> Self {
        let mut summary = Self {
            total: results.len(),
            ..Default::default()
        };
        for result in results {
            match result.status {
                GateStatus::Pass => summary.pass += 1,
                GateStatus::Fail => summary.fail += 1,
                GateStatus::Skip => summary.skip += 1,
            }
        }
        summary
    }
}

/// Threshold information for a category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdInfo {
    /// Required pass rate (0.0 to 1.0, typically 1.0).
    pub required_pass_rate: f64,
    /// Observed pass rate (None if no required gates).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_pass_rate: Option<f64>,
    /// Number of required (non-skipped) gates in this category.
    pub required_gates: usize,
    /// Number of failed gates in this category.
    pub failed_gates: usize,
}

impl ThresholdInfo {
    /// Computes threshold info for a category from gate results.
    #[must_use]
    pub fn from_results(results: &[GateResult], category: GateCategory) -> Self {
        let category_gates: Vec<_> = results.iter().filter(|r| r.category == category).collect();

        let required_gates = category_gates
            .iter()
            .filter(|r| r.status != GateStatus::Skip)
            .count();

        let pass_count = category_gates
            .iter()
            .filter(|r| r.status == GateStatus::Pass)
            .count();

        let failed_gates = category_gates
            .iter()
            .filter(|r| r.status == GateStatus::Fail)
            .count();

        let observed_pass_rate = if required_gates > 0 {
            Some(pass_count as f64 / required_gates as f64)
        } else {
            None
        };

        Self {
            required_pass_rate: 1.0,
            observed_pass_rate,
            required_gates,
            failed_gates,
        }
    }
}

/// Gate logic information for key gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateLogicEntry {
    /// Gate name.
    pub gate: String,
    /// Current status.
    pub status: String,
    /// Threshold description.
    pub threshold: String,
}

/// Gate logic section of the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateLogicInfo {
    /// Security/privacy gate status.
    pub security_privacy_gate: GateLogicEntry,
    /// Accessibility gate status.
    pub accessibility_gate: GateLogicEntry,
    /// Performance gate status.
    pub performance_gate: GateLogicEntry,
    /// Go/no-go condition description.
    pub go_condition: String,
}

impl GateLogicInfo {
    /// Constructs gate logic info from gate results.
    #[must_use]
    pub fn from_results(results: &[GateResult]) -> Self {
        let status_of = |name: &str| -> String {
            results
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.status.as_str().to_string())
                .unwrap_or_else(|| "missing".to_string())
        };

        Self {
            security_privacy_gate: GateLogicEntry {
                gate: "E2E security/privacy".to_string(),
                status: status_of("E2E security/privacy"),
                threshold: "must pass (non-quick runs)".to_string(),
            },
            accessibility_gate: GateLogicEntry {
                gate: "E2E TUI accessibility".to_string(),
                status: status_of("E2E TUI accessibility"),
                threshold: "must pass (non-quick runs)".to_string(),
            },
            performance_gate: GateLogicEntry {
                gate: "Perf + security regressions".to_string(),
                status: status_of("Perf + security regressions"),
                threshold: "must pass".to_string(),
            },
            go_condition: "all non-skipped gates pass".to_string(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Full Gate Report
// ──────────────────────────────────────────────────────────────────────────────

/// Schema version for gate reports.
pub const GATE_REPORT_SCHEMA_VERSION: &str = "am_ci_gate_report.v1";

/// Default checklist reference path.
pub const DEFAULT_CHECKLIST_REFERENCE: &str = "docs/RELEASE_CHECKLIST.md";

/// The full gate report (schema: am_ci_gate_report.v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    /// Schema version identifier.
    pub schema_version: String,
    /// ISO-8601 timestamp when report was generated.
    pub generated_at: String,
    /// Run mode (full or quick).
    pub mode: RunMode,
    /// Release decision (go or no-go).
    pub decision: Decision,
    /// Reason for the decision.
    pub decision_reason: String,
    /// Whether this run makes the release eligible.
    pub release_eligible: bool,
    /// Reference to release checklist documentation.
    pub checklist_reference: String,
    /// Summary counts (total/pass/fail/skip).
    pub summary: GateSummary,
    /// Per-category threshold information.
    pub thresholds: HashMap<GateCategory, ThresholdInfo>,
    /// Key gate logic entries.
    pub gate_logic: GateLogicInfo,
    /// Individual gate results.
    pub gates: Vec<GateResult>,
}

impl GateReport {
    /// Creates a new gate report from results.
    #[must_use]
    pub fn new(mode: RunMode, results: Vec<GateResult>) -> Self {
        let summary = GateSummary::from_results(&results);
        let gate_logic = GateLogicInfo::from_results(&results);

        // Compute thresholds for each category
        let mut thresholds = HashMap::new();
        for category in [
            GateCategory::Quality,
            GateCategory::Performance,
            GateCategory::Security,
            GateCategory::Docs,
        ] {
            thresholds.insert(category, ThresholdInfo::from_results(&results, category));
        }

        // Determine decision
        let (decision, decision_reason, release_eligible) = if summary.fail > 0 {
            (
                Decision::NoGo,
                "one or more gates failed".to_string(),
                false,
            )
        } else if mode == RunMode::Quick {
            (
                Decision::NoGo,
                "quick mode skips required release gates".to_string(),
                false,
            )
        } else {
            (
                Decision::Go,
                "all required full-run gates passed".to_string(),
                true,
            )
        };

        let generated_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        Self {
            schema_version: GATE_REPORT_SCHEMA_VERSION.to_string(),
            generated_at,
            mode,
            decision,
            decision_reason,
            release_eligible,
            checklist_reference: DEFAULT_CHECKLIST_REFERENCE.to_string(),
            summary,
            thresholds,
            gate_logic,
            gates: results,
        }
    }

    /// Creates a report with a specific timestamp (for testing).
    #[must_use]
    pub fn with_timestamp(
        mode: RunMode,
        results: Vec<GateResult>,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let mut report = Self::new(mode, results);
        report.generated_at = timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        report
    }

    /// Serializes the report to JSON.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Serializes the report to compact JSON.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn to_json_compact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Default Gates
// ──────────────────────────────────────────────────────────────────────────────

/// Returns the default set of 13 CI gates matching `scripts/ci.sh`.
#[must_use]
pub fn default_gates() -> Vec<GateConfig> {
    vec![
        // Quality gates (9)
        GateConfig::new(
            "Format check",
            GateCategory::Quality,
            ["cargo", "fmt", "--all", "--", "--check"],
        ),
        GateConfig::new(
            "Clippy",
            GateCategory::Quality,
            [
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        GateConfig::new(
            "Build workspace",
            GateCategory::Quality,
            ["cargo", "build", "--workspace"],
        ),
        GateConfig::new(
            "Unit + integration tests",
            GateCategory::Quality,
            ["cargo", "test", "--workspace"],
        ),
        GateConfig::new(
            "Mode matrix harness",
            GateCategory::Quality,
            [
                "cargo",
                "test",
                "-p",
                "mcp-agent-mail-cli",
                "--test",
                "mode_matrix_harness",
                "--",
                "--nocapture",
            ],
        ),
        GateConfig::new(
            "Semantic conformance",
            GateCategory::Quality,
            [
                "cargo",
                "test",
                "-p",
                "mcp-agent-mail-cli",
                "--test",
                "semantic_conformance",
                "--",
                "--nocapture",
            ],
        ),
        GateConfig::new(
            "Help snapshots",
            GateCategory::Quality,
            [
                "cargo",
                "test",
                "-p",
                "mcp-agent-mail-cli",
                "--test",
                "help_snapshots",
                "--",
                "--nocapture",
            ],
        ),
        GateConfig::new(
            "E2E dual-mode",
            GateCategory::Quality,
            ["bash", "scripts/e2e_dual_mode.sh"],
        )
        .skip_in_quick(),
        GateConfig::new(
            "E2E mode matrix",
            GateCategory::Quality,
            ["bash", "scripts/e2e_mode_matrix.sh"],
        )
        .skip_in_quick(),
        // Performance gate (1)
        GateConfig::new(
            "Perf + security regressions",
            GateCategory::Performance,
            [
                "cargo",
                "test",
                "-p",
                "mcp-agent-mail-cli",
                "--test",
                "perf_security_regressions",
                "--",
                "--nocapture",
            ],
        ),
        // Security gate (1)
        GateConfig::new(
            "E2E security/privacy",
            GateCategory::Security,
            ["bash", "tests/e2e/test_security_privacy.sh"],
        )
        .skip_in_quick(),
        // Docs gate (1)
        GateConfig::new(
            "Release docs references present",
            GateCategory::Docs,
            [
                "bash",
                "-c",
                "test -f docs/RELEASE_CHECKLIST.md && test -f docs/ROLLOUT_PLAYBOOK.md && test -f docs/OPERATOR_RUNBOOK.md",
            ],
        ),
        // Quality gate (E2E TUI) (1)
        GateConfig::new(
            "E2E TUI accessibility",
            GateCategory::Quality,
            ["bash", "scripts/e2e_tui_a11y.sh"],
        )
        .skip_in_quick(),
    ]
}

/// Environment variables to set on child gate processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateEnvironment {
    /// Cargo target directory.
    pub cargo_target_dir: String,
    /// SQLite database URL.
    pub database_url: String,
    /// Storage root directory.
    pub storage_root: String,
    /// Agent name for CI.
    pub agent_name: String,
    /// HTTP host binding.
    pub http_host: String,
    /// HTTP port binding.
    pub http_port: u16,
    /// HTTP path.
    pub http_path: String,
}

impl Default for GateEnvironment {
    fn default() -> Self {
        Self {
            cargo_target_dir: std::env::var("CARGO_TARGET_DIR")
                .unwrap_or_else(|_| "/data/tmp/cargo-target".to_string()),
            database_url: "sqlite:///tmp/ci_local.sqlite3".to_string(),
            storage_root: "/tmp/ci_storage".to_string(),
            agent_name: "CiLocalAgent".to_string(),
            http_host: "127.0.0.1".to_string(),
            http_port: 1,
            http_path: "/mcp/".to_string(),
        }
    }
}

impl GateEnvironment {
    /// Converts to a vector of (key, value) pairs for process environment.
    #[must_use]
    pub fn as_env_pairs(&self) -> Vec<(String, String)> {
        vec![
            (
                "CARGO_TARGET_DIR".to_string(),
                self.cargo_target_dir.clone(),
            ),
            ("DATABASE_URL".to_string(), self.database_url.clone()),
            ("STORAGE_ROOT".to_string(), self.storage_root.clone()),
            ("AGENT_NAME".to_string(), self.agent_name.clone()),
            ("HTTP_HOST".to_string(), self.http_host.clone()),
            ("HTTP_PORT".to_string(), self.http_port.to_string()),
            ("HTTP_PATH".to_string(), self.http_path.clone()),
        ]
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Gate Runner Engine
// ──────────────────────────────────────────────────────────────────────────────

/// Maximum number of stderr lines to capture for failure diagnostics.
const STDERR_TAIL_LINES: usize = 50;

/// Default timeout for a single gate execution (10 minutes).
const DEFAULT_GATE_TIMEOUT_SECS: u64 = 600;

/// Error returned by gate runner operations.
#[derive(Debug)]
pub enum GateRunnerError {
    /// Failed to spawn the subprocess.
    SpawnFailed(std::io::Error),
    /// Command timed out.
    Timeout { elapsed_secs: u64 },
    /// Other I/O error.
    Io(std::io::Error),
}

impl std::fmt::Display for GateRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpawnFailed(e) => write!(f, "failed to spawn subprocess: {e}"),
            Self::Timeout { elapsed_secs } => {
                write!(f, "gate timed out after {elapsed_secs}s")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for GateRunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnFailed(e) | Self::Io(e) => Some(e),
            Self::Timeout { .. } => None,
        }
    }
}

/// Gate runner configuration.
#[derive(Debug, Clone)]
pub struct GateRunnerConfig {
    /// Working directory for gate execution.
    pub working_dir: std::path::PathBuf,
    /// Environment variables to set.
    pub env: GateEnvironment,
    /// Timeout per gate in seconds.
    pub timeout_secs: u64,
    /// Run mode (full or quick).
    pub mode: RunMode,
    /// Callback for progress reporting (gate name, index, total).
    pub on_gate_start: Option<fn(&str, usize, usize)>,
    /// Callback for result reporting.
    pub on_gate_complete: Option<fn(&GateResult)>,
}

impl Default for GateRunnerConfig {
    fn default() -> Self {
        Self {
            working_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            env: GateEnvironment::default(),
            timeout_secs: DEFAULT_GATE_TIMEOUT_SECS,
            mode: RunMode::Full,
            on_gate_start: None,
            on_gate_complete: None,
        }
    }
}

impl GateRunnerConfig {
    /// Creates a new config with the given working directory.
    #[must_use]
    pub fn new(working_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            ..Self::default()
        }
    }

    /// Builder: set run mode.
    #[must_use]
    pub fn mode(mut self, mode: RunMode) -> Self {
        self.mode = mode;
        self
    }

    /// Builder: set timeout per gate.
    #[must_use]
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Builder: set environment.
    #[must_use]
    pub fn env(mut self, env: GateEnvironment) -> Self {
        self.env = env;
        self
    }
}

/// Runs a single gate and returns the result.
///
/// # Arguments
/// * `config` - The gate configuration to execute.
/// * `runner_config` - Runner configuration (working dir, env, timeout).
///
/// # Returns
/// A `GateResult` with pass/fail/skip status and timing.
pub fn run_gate(config: &GateConfig, runner_config: &GateRunnerConfig) -> GateResult {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    // Skip if in quick mode and gate is marked skip_in_quick
    if runner_config.mode == RunMode::Quick && config.skip_in_quick {
        return GateResult::skip(config, "--quick mode");
    }

    // Validate command has at least one element
    if config.command.is_empty() {
        return GateResult::fail(config, Duration::ZERO, Some("empty command".to_string()));
    }

    let start = Instant::now();

    // Build the command
    let mut cmd = Command::new(&config.command[0]);
    if config.command.len() > 1 {
        cmd.args(&config.command[1..]);
    }

    // Set working directory
    cmd.current_dir(&runner_config.working_dir);

    // Set environment variables
    for (key, value) in runner_config.env.as_env_pairs() {
        cmd.env(key, value);
    }

    // Capture stdout/stderr
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Spawn the process
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let elapsed = start.elapsed();
            return GateResult::fail(config, elapsed, Some(format!("spawn failed: {e}")));
        }
    };

    // Capture stderr in background
    let stderr_handle = child.stderr.take();
    let stderr_lines: Vec<String> = if let Some(stderr) = stderr_handle {
        let reader = BufReader::new(stderr);
        reader.lines().filter_map(Result::ok).collect()
    } else {
        Vec::new()
    };

    // Wait for completion with timeout
    let timeout = Duration::from_secs(runner_config.timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if start.elapsed() > timeout {
                    // Kill the process on timeout
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(GateRunnerError::Timeout {
                        elapsed_secs: start.elapsed().as_secs(),
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => break Err(GateRunnerError::Io(e)),
        }
    };

    let elapsed = start.elapsed();

    match status {
        Ok(exit_status) if exit_status.success() => GateResult::pass(config, elapsed),
        Ok(_exit_status) => {
            // Capture last N lines of stderr for diagnostics
            let tail: Vec<_> = stderr_lines
                .iter()
                .rev()
                .take(STDERR_TAIL_LINES)
                .rev()
                .cloned()
                .collect();
            let stderr_tail = if tail.is_empty() {
                None
            } else {
                Some(tail.join("\n"))
            };
            GateResult::fail(config, elapsed, stderr_tail)
        }
        Err(GateRunnerError::Timeout { elapsed_secs }) => GateResult::fail(
            config,
            Duration::from_secs(elapsed_secs),
            Some(format!("timeout after {}s", elapsed_secs)),
        ),
        Err(e) => GateResult::fail(config, elapsed, Some(format!("{e}"))),
    }
}

/// Runs all gates sequentially and returns a report.
///
/// # Arguments
/// * `gates` - List of gate configurations to run.
/// * `runner_config` - Runner configuration.
///
/// # Returns
/// A `GateReport` with all results and summary.
pub fn run_gates(gates: &[GateConfig], runner_config: &GateRunnerConfig) -> GateReport {
    let total = gates.len();
    let mut results = Vec::with_capacity(total);

    for (idx, gate) in gates.iter().enumerate() {
        // Progress callback
        if let Some(callback) = runner_config.on_gate_start {
            callback(&gate.name, idx, total);
        }

        // Run the gate
        let result = run_gate(gate, runner_config);

        // Result callback
        if let Some(callback) = runner_config.on_gate_complete {
            callback(&result);
        }

        results.push(result);
    }

    GateReport::new(runner_config.mode, results)
}

/// Runs all default gates and returns a report.
///
/// This is a convenience function that combines `default_gates()` with `run_gates()`.
///
/// # Arguments
/// * `runner_config` - Runner configuration.
///
/// # Returns
/// A `GateReport` with all results.
pub fn run_default_gates(runner_config: &GateRunnerConfig) -> GateReport {
    let gates = default_gates();
    run_gates(&gates, runner_config)
}

/// Prints a human-readable summary of gate results to stdout.
pub fn print_gate_summary(report: &GateReport) {
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  CI GATE REPORT");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    for result in &report.gates {
        let status_icon = match result.status {
            GateStatus::Pass => "✓",
            GateStatus::Fail => "✗",
            GateStatus::Skip => "⊘",
        };
        let status_label = match result.status {
            GateStatus::Pass => "PASS",
            GateStatus::Fail => "FAIL",
            GateStatus::Skip => "SKIP",
        };
        println!(
            "  {} {} [{}s] {}",
            status_icon, status_label, result.elapsed_seconds, result.name
        );
        if let Some(ref tail) = result.stderr_tail {
            // Print first 3 lines of stderr for quick diagnostics
            for line in tail.lines().take(3) {
                println!("      {}", line);
            }
        }
    }

    println!();
    println!(
        "Summary: {} total, {} pass, {} fail, {} skip",
        report.summary.total, report.summary.pass, report.summary.fail, report.summary.skip
    );
    println!("Decision: {}", report.decision);
    println!("Total time: {}s", report.total_elapsed_seconds);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gate_category_serialization() {
        assert_eq!(
            serde_json::to_string(&GateCategory::Quality).unwrap(),
            "\"quality\""
        );
        assert_eq!(
            serde_json::to_string(&GateCategory::Performance).unwrap(),
            "\"performance\""
        );
        assert_eq!(
            serde_json::to_string(&GateCategory::Security).unwrap(),
            "\"security\""
        );
        assert_eq!(
            serde_json::to_string(&GateCategory::Docs).unwrap(),
            "\"docs\""
        );
    }

    #[test]
    fn test_gate_status_serialization() {
        assert_eq!(
            serde_json::to_string(&GateStatus::Pass).unwrap(),
            "\"pass\""
        );
        assert_eq!(
            serde_json::to_string(&GateStatus::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&GateStatus::Skip).unwrap(),
            "\"skip\""
        );
    }

    #[test]
    fn test_decision_serialization() {
        assert_eq!(serde_json::to_string(&Decision::Go).unwrap(), "\"go\"");
        assert_eq!(serde_json::to_string(&Decision::NoGo).unwrap(), "\"no-go\"");
    }

    #[test]
    fn test_run_mode_serialization() {
        assert_eq!(serde_json::to_string(&RunMode::Full).unwrap(), "\"full\"");
        assert_eq!(serde_json::to_string(&RunMode::Quick).unwrap(), "\"quick\"");
    }

    #[test]
    fn test_default_gates_count() {
        let gates = default_gates();
        assert_eq!(gates.len(), 13, "Expected 13 default gates");
    }

    #[test]
    fn test_default_gates_skip_in_quick() {
        let gates = default_gates();
        let quick_skip: Vec<_> = gates.iter().filter(|g| g.skip_in_quick).collect();
        assert_eq!(
            quick_skip.len(),
            4,
            "Expected 4 gates to skip in quick mode"
        );

        let names: Vec<_> = quick_skip.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"E2E dual-mode"));
        assert!(names.contains(&"E2E mode matrix"));
        assert!(names.contains(&"E2E security/privacy"));
        assert!(names.contains(&"E2E TUI accessibility"));
    }

    #[test]
    fn test_gate_config_builder() {
        let gate = GateConfig::new("Test gate", GateCategory::Quality, ["cargo", "test"])
            .skip_in_quick()
            .parallel_group("group-a");

        assert_eq!(gate.name, "Test gate");
        assert!(gate.skip_in_quick);
        assert_eq!(gate.parallel_group, Some("group-a".to_string()));
    }

    #[test]
    fn test_gate_summary_from_results() {
        let results = vec![
            GateResult {
                name: "Gate 1".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 10,
                command: "test".to_string(),
                stderr_tail: None,
            },
            GateResult {
                name: "Gate 2".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Fail,
                elapsed_seconds: 5,
                command: "test".to_string(),
                stderr_tail: Some("error".to_string()),
            },
            GateResult {
                name: "Gate 3".to_string(),
                category: GateCategory::Security,
                status: GateStatus::Skip,
                elapsed_seconds: 0,
                command: "--quick".to_string(),
                stderr_tail: None,
            },
        ];

        let summary = GateSummary::from_results(&results);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.pass, 1);
        assert_eq!(summary.fail, 1);
        assert_eq!(summary.skip, 1);
    }

    #[test]
    fn test_threshold_info_calculation() {
        let results = vec![
            GateResult {
                name: "Quality 1".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 10,
                command: "test".to_string(),
                stderr_tail: None,
            },
            GateResult {
                name: "Quality 2".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 5,
                command: "test".to_string(),
                stderr_tail: None,
            },
            GateResult {
                name: "Quality 3".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Skip,
                elapsed_seconds: 0,
                command: "--quick".to_string(),
                stderr_tail: None,
            },
        ];

        let threshold = ThresholdInfo::from_results(&results, GateCategory::Quality);
        assert_eq!(threshold.required_gates, 2); // 2 non-skipped
        assert_eq!(threshold.failed_gates, 0);
        assert_eq!(threshold.observed_pass_rate, Some(1.0)); // 2/2 passed
    }

    #[test]
    fn test_gate_report_go_decision() {
        let results = vec![GateResult {
            name: "Gate 1".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 10,
            command: "test".to_string(),
            stderr_tail: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        assert_eq!(report.decision, Decision::Go);
        assert!(report.release_eligible);
    }

    #[test]
    fn test_gate_report_no_go_on_failure() {
        let results = vec![GateResult {
            name: "Gate 1".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Fail,
            elapsed_seconds: 10,
            command: "test".to_string(),
            stderr_tail: Some("compilation error".to_string()),
        }];

        let report = GateReport::new(RunMode::Full, results);
        assert_eq!(report.decision, Decision::NoGo);
        assert!(!report.release_eligible);
        assert_eq!(report.decision_reason, "one or more gates failed");
    }

    #[test]
    fn test_gate_report_no_go_on_quick_mode() {
        let results = vec![GateResult {
            name: "Gate 1".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 10,
            command: "test".to_string(),
            stderr_tail: None,
        }];

        let report = GateReport::new(RunMode::Quick, results);
        assert_eq!(report.decision, Decision::NoGo);
        assert!(!report.release_eligible);
        assert_eq!(
            report.decision_reason,
            "quick mode skips required release gates"
        );
    }

    #[test]
    fn test_gate_report_json_serialization() {
        let results = vec![GateResult {
            name: "Format check".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 2,
            command: "cargo fmt --all -- --check".to_string(),
            stderr_tail: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        let json = report.to_json().expect("serialization should succeed");

        assert!(json.contains("\"schema_version\": \"am_ci_gate_report.v1\""));
        assert!(json.contains("\"decision\": \"go\""));
    }

    #[test]
    fn test_gate_environment_defaults() {
        let env = GateEnvironment::default();
        assert_eq!(env.database_url, "sqlite:///tmp/ci_local.sqlite3");
        assert_eq!(env.storage_root, "/tmp/ci_storage");
        assert_eq!(env.agent_name, "CiLocalAgent");
        assert_eq!(env.http_host, "127.0.0.1");
        assert_eq!(env.http_port, 1);
        assert_eq!(env.http_path, "/mcp/");
    }

    #[test]
    fn test_gate_environment_as_env_pairs() {
        let env = GateEnvironment::default();
        let pairs = env.as_env_pairs();

        assert_eq!(pairs.len(), 7);
        assert!(pairs.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"));
        assert!(pairs.iter().any(|(k, _)| k == "DATABASE_URL"));
    }

    // ── Gate Runner Tests ────────────────────────────────────────────────────

    #[test]
    fn test_gate_runner_config_default() {
        let config = GateRunnerConfig::default();
        assert_eq!(config.mode, RunMode::Full);
        assert_eq!(config.timeout_secs, DEFAULT_GATE_TIMEOUT_SECS);
    }

    #[test]
    fn test_gate_runner_config_builder() {
        let config = GateRunnerConfig::new("/tmp/test")
            .mode(RunMode::Quick)
            .timeout_secs(120);

        assert_eq!(config.mode, RunMode::Quick);
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.working_dir, std::path::PathBuf::from("/tmp/test"));
    }

    #[test]
    fn test_run_gate_skips_in_quick_mode() {
        let gate = GateConfig::new("E2E test", GateCategory::Quality, ["true"]).skip_in_quick();
        let config = GateRunnerConfig::default().mode(RunMode::Quick);

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Skip);
        assert_eq!(result.command, "--quick mode");
    }

    #[test]
    fn test_run_gate_passes_simple_command() {
        let gate = GateConfig::new("Echo test", GateCategory::Quality, ["true"]);
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Pass);
        assert_eq!(result.name, "Echo test");
        assert!(result.stderr_tail.is_none());
    }

    #[test]
    fn test_run_gate_fails_on_bad_exit() {
        let gate = GateConfig::new("Fail test", GateCategory::Quality, ["false"]);
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
    }

    #[test]
    fn test_run_gate_captures_stderr() {
        let gate = GateConfig::new(
            "Stderr test",
            GateCategory::Quality,
            ["bash", "-c", "echo 'error message' >&2 && exit 1"],
        );
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        assert!(result.stderr_tail.is_some());
        assert!(result.stderr_tail.unwrap().contains("error message"));
    }

    #[test]
    fn test_run_gate_empty_command() {
        let gate = GateConfig {
            name: "Empty".to_string(),
            category: GateCategory::Quality,
            command: vec![],
            skip_in_quick: false,
            parallel_group: None,
        };
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        assert!(result
            .stderr_tail
            .as_ref()
            .unwrap()
            .contains("empty command"));
    }

    #[test]
    fn test_run_gates_multiple() {
        let gates = vec![
            GateConfig::new("Pass 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Pass 2", GateCategory::Quality, ["true"]),
            GateConfig::new("Skip me", GateCategory::Quality, ["true"]).skip_in_quick(),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Quick);

        let report = run_gates(&gates, &config);

        assert_eq!(report.summary.total, 3);
        assert_eq!(report.summary.pass, 2);
        assert_eq!(report.summary.skip, 1);
        assert_eq!(report.decision, Decision::NoGo); // quick mode = no-go
    }

    #[test]
    fn test_run_gates_all_pass() {
        let gates = vec![
            GateConfig::new("Pass 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Pass 2", GateCategory::Quality, ["true"]),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let report = run_gates(&gates, &config);

        assert_eq!(report.summary.pass, 2);
        assert_eq!(report.decision, Decision::Go);
    }

    #[test]
    fn test_gate_runner_error_display() {
        let err = GateRunnerError::Timeout { elapsed_secs: 120 };
        assert_eq!(format!("{err}"), "gate timed out after 120s");

        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err = GateRunnerError::SpawnFailed(io_err);
        assert!(format!("{err}").contains("failed to spawn subprocess"));
    }
}
