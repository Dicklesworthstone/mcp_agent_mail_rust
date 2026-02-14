//! CI gate configuration data model for `am ci` command.
//!
//! This module defines the canonical data contracts for the native CI command path,
//! replacing the implicit schema previously encoded in the legacy `ci.sh` script.
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
    /// Expected artifact paths/globs produced by this gate.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub expected_artifacts: Vec<String>,
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
            expected_artifacts: Vec::new(),
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

    /// Builder: attach one expected artifact path or glob.
    #[must_use]
    pub fn expected_artifact(mut self, artifact: impl Into<String>) -> Self {
        self.expected_artifacts.push(artifact.into());
        self
    }

    /// Builder: attach multiple expected artifact paths/globs.
    #[must_use]
    pub fn expected_artifacts(
        mut self,
        artifacts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.expected_artifacts
            .extend(artifacts.into_iter().map(Into::into));
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

// ──────────────────────────────────────────────────────────────────────────────
// Gate Error Classification
// ──────────────────────────────────────────────────────────────────────────────

/// Structured error information extracted from gate stderr.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GateError {
    /// Last N lines of stderr output.
    pub stderr_tail: String,
    /// Number of errors detected (if parseable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u32>,
    /// One-line summary of the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_summary: Option<String>,
    /// Files mentioned in error output.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affected_files: Vec<String>,
    /// Error category (compiler, test, clippy, format, unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

impl GateError {
    /// Maximum number of lines to retain in stderr_tail.
    const MAX_STDERR_LINES: usize = 100;

    /// Creates a GateError from raw stderr output with error classification.
    #[must_use]
    pub fn from_stderr(stderr: &str) -> Self {
        let lines: Vec<&str> = stderr.lines().collect();
        let tail_lines = if lines.len() > Self::MAX_STDERR_LINES {
            &lines[lines.len() - Self::MAX_STDERR_LINES..]
        } else {
            &lines[..]
        };
        let stderr_tail = tail_lines.join("\n");

        let (error_category, error_count, error_summary) = classify_stderr(stderr);
        let affected_files = extract_affected_files(stderr);

        Self {
            stderr_tail,
            error_count,
            error_summary,
            affected_files,
            error_category,
        }
    }

    /// Creates a simple GateError with just a message (no classification).
    #[must_use]
    pub fn simple(message: impl Into<String>) -> Self {
        Self::simple_with_category(message, None::<String>)
    }

    /// Creates a simple GateError with an optional explicit category.
    #[must_use]
    pub fn simple_with_category(
        message: impl Into<String>,
        category: Option<impl Into<String>>,
    ) -> Self {
        let msg = message.into();
        Self {
            stderr_tail: msg.clone(),
            error_summary: Some(msg),
            error_category: category.map(Into::into),
            ..Default::default()
        }
    }

    /// Creates an artifact verification error for missing or partial outputs.
    #[must_use]
    pub fn artifact_validation(found: &[String], missing: &[String]) -> Self {
        let category = if found.is_empty() {
            "artifact_missing"
        } else {
            "artifact_partial"
        };
        let missing_list = missing.join(", ");
        let summary = if found.is_empty() {
            format!("required artifact(s) missing: {missing_list}")
        } else {
            format!("partial artifact output: missing {}", missing_list)
        };
        let mut stderr_tail = summary.clone();
        if !found.is_empty() {
            stderr_tail.push_str("\nfound artifacts: ");
            stderr_tail.push_str(&found.join(", "));
        }

        Self {
            stderr_tail,
            error_count: u32::try_from(missing.len()).ok(),
            error_summary: Some(summary),
            affected_files: missing.to_vec(),
            error_category: Some(category.to_string()),
        }
    }
}

/// Classifies stderr output and extracts error information.
///
/// Returns (category, error_count, error_summary).
fn classify_stderr(stderr: &str) -> (Option<String>, Option<u32>, Option<String>) {
    // Rust compiler error pattern: error[E0XXX]: message
    let compiler_pattern = regex::Regex::new(r"^error\[E\d+\]: (.+)$").ok();
    // Test failure patterns
    let test_panic = regex::Regex::new(r"thread '(.+)' panicked").ok();
    let test_failed = regex::Regex::new(r"^---- (.+) ----$").ok();
    // Clippy warning/error
    let clippy_pattern = regex::Regex::new(r"^warning: (.+)$").ok();
    // rustfmt diff
    let format_pattern = regex::Regex::new(r"^Diff in (.+):$").ok();

    let mut compiler_errors = 0u32;
    let mut test_failures = 0u32;
    let mut clippy_warnings = 0u32;
    let mut format_diffs = 0u32;
    let mut first_compiler_msg: Option<String> = None;
    let mut first_test_name: Option<String> = None;
    let mut first_clippy_msg: Option<String> = None;
    let mut first_format_file: Option<String> = None;

    for line in stderr.lines() {
        // Compiler errors
        if let Some(ref re) = compiler_pattern {
            if let Some(caps) = re.captures(line) {
                compiler_errors += 1;
                if first_compiler_msg.is_none() {
                    first_compiler_msg = caps.get(1).map(|m| m.as_str().to_string());
                }
            }
        }

        // Test panics
        if let Some(ref re) = test_panic {
            if let Some(caps) = re.captures(line) {
                test_failures += 1;
                if first_test_name.is_none() {
                    first_test_name = caps.get(1).map(|m| m.as_str().to_string());
                }
            }
        }

        // Test failures (---- test_name ----)
        if let Some(ref re) = test_failed {
            if let Some(caps) = re.captures(line) {
                test_failures += 1;
                if first_test_name.is_none() {
                    first_test_name = caps.get(1).map(|m| m.as_str().to_string());
                }
            }
        }

        // Clippy warnings
        if let Some(ref re) = clippy_pattern {
            if let Some(caps) = re.captures(line) {
                clippy_warnings += 1;
                if first_clippy_msg.is_none() {
                    first_clippy_msg = caps.get(1).map(|m| m.as_str().to_string());
                }
            }
        }

        // Format diffs
        if let Some(ref re) = format_pattern {
            if let Some(caps) = re.captures(line) {
                format_diffs += 1;
                if first_format_file.is_none() {
                    first_format_file = caps.get(1).map(|m| m.as_str().to_string());
                }
            }
        }
    }

    // Determine primary error category (priority order)
    if compiler_errors > 0 {
        let summary = first_compiler_msg
            .map(|m| format!("{} compiler error(s): {}", compiler_errors, m))
            .unwrap_or_else(|| format!("{} compiler error(s)", compiler_errors));
        (
            Some("compiler".to_string()),
            Some(compiler_errors),
            Some(summary),
        )
    } else if test_failures > 0 {
        let summary = first_test_name
            .map(|t| format!("{} test failure(s), first: {}", test_failures, t))
            .unwrap_or_else(|| format!("{} test failure(s)", test_failures));
        (Some("test".to_string()), Some(test_failures), Some(summary))
    } else if clippy_warnings > 0 {
        let summary = first_clippy_msg
            .map(|m| format!("{} clippy warning(s): {}", clippy_warnings, m))
            .unwrap_or_else(|| format!("{} clippy warning(s)", clippy_warnings));
        (
            Some("clippy".to_string()),
            Some(clippy_warnings),
            Some(summary),
        )
    } else if format_diffs > 0 {
        let summary = first_format_file
            .map(|f| format!("{} file(s) need formatting, first: {}", format_diffs, f))
            .unwrap_or_else(|| format!("{} file(s) need formatting", format_diffs));
        (
            Some("format".to_string()),
            Some(format_diffs),
            Some(summary),
        )
    } else if !stderr.trim().is_empty() {
        // Unknown error with non-empty stderr
        let first_line = stderr.lines().next().unwrap_or("unknown error");
        (
            Some("unknown".to_string()),
            None,
            Some(first_line.to_string()),
        )
    } else {
        (None, None, None)
    }
}

/// Extracts file paths mentioned in stderr output.
fn extract_affected_files(stderr: &str) -> Vec<String> {
    // Pattern for Rust file references: path/to/file.rs:line:col
    let file_pattern = regex::Regex::new(r"(?:^|\s)([\w./\-]+\.rs):(\d+)").ok();

    let mut files = std::collections::HashSet::new();

    if let Some(ref re) = file_pattern {
        for caps in re.captures_iter(stderr) {
            if let Some(path) = caps.get(1) {
                let path_str = path.as_str();
                // Filter out common false positives
                if !path_str.starts_with("http") && !path_str.contains("://") {
                    files.insert(path_str.to_string());
                }
            }
        }
    }

    let mut result: Vec<_> = files.into_iter().collect();
    result.sort();
    result
}

fn has_glob_tokens(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn normalize_artifact_path(path: &std::path::Path, base: &std::path::Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn resolve_expected_artifacts(
    working_dir: &std::path::Path,
    expected_artifacts: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut found = std::collections::BTreeSet::new();
    let mut missing = Vec::new();

    for pattern in expected_artifacts {
        if has_glob_tokens(pattern) {
            let abs_pattern = working_dir.join(pattern);
            let abs_pattern = abs_pattern.to_string_lossy().to_string();
            let mut matched = false;

            if let Ok(paths) = glob::glob(&abs_pattern) {
                for path in paths.flatten() {
                    matched = true;
                    found.insert(normalize_artifact_path(&path, working_dir));
                }
            }

            if !matched {
                missing.push(pattern.clone());
            }
        } else {
            let candidate = working_dir.join(pattern);
            if candidate.exists() {
                found.insert(normalize_artifact_path(&candidate, working_dir));
            } else {
                missing.push(pattern.clone());
            }
        }
    }

    (found.into_iter().collect(), missing)
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
    /// Deprecated: use `error.stderr_tail` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// Structured error information (only present for failed gates).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<GateError>,
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
            error: None,
        }
    }

    /// Creates a fail result with optional stderr.
    ///
    /// When `stderr` is provided, it is parsed to extract structured error
    /// information including error count, summary, and affected files.
    #[must_use]
    pub fn fail(config: &GateConfig, elapsed: Duration, stderr: Option<String>) -> Self {
        let (stderr_tail, error) = match stderr {
            Some(ref s) if !s.is_empty() => {
                let gate_error = GateError::from_stderr(s);
                // Keep stderr_tail for backward compatibility
                (Some(gate_error.stderr_tail.clone()), Some(gate_error))
            }
            _ => (None, None),
        };

        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Fail,
            elapsed_seconds: elapsed.as_secs(),
            command: config.command_display(),
            stderr_tail,
            error,
        }
    }

    /// Creates a fail result with a pre-built structured error.
    #[must_use]
    pub fn fail_with_error(config: &GateConfig, elapsed: Duration, error: GateError) -> Self {
        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Fail,
            elapsed_seconds: elapsed.as_secs(),
            command: config.command_display(),
            stderr_tail: Some(error.stderr_tail.clone()),
            error: Some(error),
        }
    }

    /// Creates a fail result with a simple error message (no stderr parsing).
    #[must_use]
    pub fn fail_simple(config: &GateConfig, elapsed: Duration, message: impl Into<String>) -> Self {
        Self::fail_simple_with_category(config, elapsed, message, None::<String>)
    }

    /// Creates a fail result with a simple error message and optional category.
    #[must_use]
    pub fn fail_simple_with_category(
        config: &GateConfig,
        elapsed: Duration,
        message: impl Into<String>,
        category: Option<impl Into<String>>,
    ) -> Self {
        let msg = message.into();
        Self {
            name: config.name.clone(),
            category: config.category,
            status: GateStatus::Fail,
            elapsed_seconds: elapsed.as_secs(),
            command: config.command_display(),
            stderr_tail: Some(msg.clone()),
            error: Some(GateError::simple_with_category(msg, category)),
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
            error: None,
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

/// Canonical execution log entry for one gate run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateExecutionLogEntry {
    /// Gate name.
    pub gate: String,
    /// Command line that was executed (or skip reason).
    pub command: String,
    /// Duration in seconds.
    pub elapsed_seconds: u64,
    /// Normalized exit code for reporting:
    /// 0 = pass, 1 = fail, 124 = timeout, -1 = skipped.
    pub normalized_exit_code: i32,
    /// Normalized error category (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    /// Artifact/log bundle paths associated with this gate.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub artifact_links: Vec<String>,
}

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
    /// Total elapsed time across all gates in seconds.
    pub total_elapsed_seconds: u64,
    /// Summary counts (total/pass/fail/skip).
    pub summary: GateSummary,
    /// Per-category pass/fail/skip breakdown.
    pub category_breakdown: HashMap<GateCategory, GateSummary>,
    /// Per-category threshold information.
    pub thresholds: HashMap<GateCategory, ThresholdInfo>,
    /// Key gate logic entries.
    pub gate_logic: GateLogicInfo,
    /// Artifact/log bundle links keyed by gate name.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub artifact_links: HashMap<String, Vec<String>>,
    /// Canonical execution logs for each gate.
    pub execution_log: Vec<GateExecutionLogEntry>,
    /// Individual gate results.
    pub gates: Vec<GateResult>,
}

impl GateReport {
    /// Creates a new gate report from results.
    #[must_use]
    pub fn new(mode: RunMode, results: Vec<GateResult>) -> Self {
        Self::new_with_gate_configs(mode, results, &[])
    }

    fn category_breakdown(results: &[GateResult]) -> HashMap<GateCategory, GateSummary> {
        let mut breakdown = HashMap::new();
        for category in [
            GateCategory::Quality,
            GateCategory::Performance,
            GateCategory::Security,
            GateCategory::Docs,
        ] {
            let category_results: Vec<_> = results
                .iter()
                .filter(|result| result.category == category)
                .cloned()
                .collect();
            if !category_results.is_empty() {
                breakdown.insert(category, GateSummary::from_results(&category_results));
            }
        }
        breakdown
    }

    fn normalized_exit_code(result: &GateResult) -> i32 {
        match result.status {
            GateStatus::Pass => 0,
            GateStatus::Skip => -1,
            GateStatus::Fail => {
                if result
                    .error
                    .as_ref()
                    .and_then(|err| err.error_category.as_deref())
                    == Some("timeout")
                {
                    124
                } else {
                    1
                }
            }
        }
    }

    /// Creates a new gate report from results with gate config metadata.
    #[must_use]
    pub fn new_with_gate_configs(
        mode: RunMode,
        results: Vec<GateResult>,
        gate_configs: &[GateConfig],
    ) -> Self {
        let summary = GateSummary::from_results(&results);
        let category_breakdown = Self::category_breakdown(&results);
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
        let total_elapsed_seconds = results.iter().map(|r| r.elapsed_seconds).sum();
        let artifact_links: HashMap<_, _> = gate_configs
            .iter()
            .filter(|gate| !gate.expected_artifacts.is_empty())
            .map(|gate| (gate.name.clone(), gate.expected_artifacts.clone()))
            .collect();
        let execution_log = results
            .iter()
            .map(|result| GateExecutionLogEntry {
                gate: result.name.clone(),
                command: result.command.clone(),
                elapsed_seconds: result.elapsed_seconds,
                normalized_exit_code: Self::normalized_exit_code(result),
                error_category: result
                    .error
                    .as_ref()
                    .and_then(|error| error.error_category.clone()),
                artifact_links: artifact_links
                    .get(&result.name)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();

        Self {
            schema_version: GATE_REPORT_SCHEMA_VERSION.to_string(),
            generated_at,
            mode,
            decision,
            decision_reason,
            release_eligible,
            checklist_reference: DEFAULT_CHECKLIST_REFERENCE.to_string(),
            total_elapsed_seconds,
            summary,
            category_breakdown,
            thresholds,
            gate_logic,
            artifact_links,
            execution_log,
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

    /// Writes the report to a file as pretty-printed JSON.
    ///
    /// # Arguments
    /// * `path` - The file path to write to.
    ///
    /// # Errors
    /// Returns an error if serialization or file writing fails.
    pub fn write_to_file(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        use std::io::Write;
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = std::fs::File::create(path)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// Reads a report from a JSON file.
    ///
    /// # Arguments
    /// * `path` - The file path to read from.
    ///
    /// # Errors
    /// Returns an error if file reading or deserialization fails.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Parses a report from a JSON string.
    ///
    /// # Errors
    /// Returns an error if deserialization fails.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Returns the list of failed gates.
    #[must_use]
    pub fn failed_gates(&self) -> Vec<&GateResult> {
        self.gates
            .iter()
            .filter(|g| g.status == GateStatus::Fail)
            .collect()
    }

    /// Returns the list of skipped gates.
    #[must_use]
    pub fn skipped_gates(&self) -> Vec<&GateResult> {
        self.gates
            .iter()
            .filter(|g| g.status == GateStatus::Skip)
            .collect()
    }

    /// Returns a formatted failure summary for triage.
    #[must_use]
    pub fn failure_summary(&self) -> String {
        let failed = self.failed_gates();
        if failed.is_empty() {
            return String::from("No failures.");
        }

        let mut lines = vec![format!("{} gate(s) failed:", failed.len())];
        for gate in failed {
            lines.push(format!("  - {} [{}]", gate.name, gate.category));
            if let Some(ref tail) = gate.stderr_tail {
                // Include first line of stderr for quick context
                if let Some(first_line) = tail.lines().next() {
                    lines.push(format!("      {}", first_line));
                }
            }
        }
        lines.join("\n")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Default Gates
// ──────────────────────────────────────────────────────────────────────────────

/// Returns the default set of 15 CI gates.
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
            "DB stress suite",
            GateCategory::Quality,
            [
                "cargo",
                "test",
                "-p",
                "mcp-agent-mail-db",
                "--test",
                "stress",
                "--",
                "--nocapture",
            ],
        )
        .skip_in_quick(),
        GateConfig::new(
            "E2E full matrix",
            GateCategory::Quality,
            ["bash", "scripts/e2e_test.sh"],
        )
        .expected_artifact("tests/artifacts_native")
        .skip_in_quick(),
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
        .expected_artifact("tests/artifacts/dual_mode/*")
        .skip_in_quick(),
        GateConfig::new(
            "E2E mode matrix",
            GateCategory::Quality,
            ["bash", "scripts/e2e_mode_matrix.sh"],
        )
        .expected_artifact("tests/artifacts/mode_matrix/*")
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
        .expected_artifact("tests/artifacts/security_privacy/*")
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
        .expected_artifact("tests/artifacts/tui_a11y/*")
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
    use std::io::{self, BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    // Skip if in quick mode and gate is marked skip_in_quick
    if runner_config.mode == RunMode::Quick && config.skip_in_quick {
        return GateResult::skip(config, "--quick mode");
    }

    // Validate command has at least one element
    if config.command.is_empty() {
        return GateResult::fail_simple(config, Duration::ZERO, "empty command");
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
            return GateResult::fail_simple(config, elapsed, format!("spawn failed: {e}"));
        }
    };

    // Drain stdout/stderr in background so child processes cannot block
    // on full pipe buffers. Keep stderr lines for diagnostics.
    let stdout_drain = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut sink = io::sink();
            let _ = io::copy(&mut reader, &mut sink);
        })
    });
    let stderr_reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            reader.lines().map_while(Result::ok).collect::<Vec<_>>()
        })
    });

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
    let stderr_lines = stderr_reader
        .and_then(|join| join.join().ok())
        .unwrap_or_default();
    if let Some(join) = stdout_drain {
        let _ = join.join();
    }

    match status {
        Ok(exit_status) if exit_status.success() => {
            let (found, missing) =
                resolve_expected_artifacts(&runner_config.working_dir, &config.expected_artifacts);
            if missing.is_empty() {
                GateResult::pass(config, elapsed)
            } else {
                GateResult::fail_with_error(
                    config,
                    elapsed,
                    GateError::artifact_validation(&found, &missing),
                )
            }
        }
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
        Err(GateRunnerError::Timeout { elapsed_secs }) => GateResult::fail_simple_with_category(
            config,
            Duration::from_secs(elapsed_secs),
            format!("timeout after {}s", elapsed_secs),
            Some("timeout"),
        ),
        Err(e) => GateResult::fail_simple(config, elapsed, format!("{e}")),
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

    GateReport::new_with_gate_configs(runner_config.mode, results, gates)
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

/// Runs all gates in parallel where possible and returns a report.
///
/// Gates are run concurrently using a thread pool. Each gate runs in its own
/// subprocess, so isolation is maintained. Stdout/stderr are captured per-gate
/// to avoid interleaving.
///
/// The compile group (fmt, clippy, build) must complete before the test group
/// runs to ensure build artifacts exist. Other groups can run in parallel.
///
/// # Arguments
/// * `gates` - List of gate configurations to run.
/// * `runner_config` - Runner configuration.
///
/// # Returns
/// A `GateReport` with all results and summary.
pub fn run_gates_parallel(gates: &[GateConfig], runner_config: &GateRunnerConfig) -> GateReport {
    use std::sync::{Arc, Mutex};
    use std::thread;

    let total = gates.len();

    // Separate gates into phases:
    // Phase 1: Compile gates (must run first and complete)
    // Phase 2: All other gates (can run in parallel)
    let compile_gates: Vec<_> = gates
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            g.name == "Format check" || g.name == "Clippy" || g.name == "Build workspace"
        })
        .collect();

    let other_gates: Vec<_> = gates
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            g.name != "Format check" && g.name != "Clippy" && g.name != "Build workspace"
        })
        .collect();

    let results: Arc<Mutex<Vec<(usize, GateResult)>>> = Arc::new(Mutex::new(Vec::new()));

    // Phase 1: Run compile gates in parallel
    eprintln!("Phase 1: Running compile gates in parallel...");
    {
        let handles: Vec<_> = compile_gates
            .iter()
            .map(|(idx, gate)| {
                if let Some(callback) = runner_config.on_gate_start {
                    callback(&gate.name, *idx, total);
                }
                let gate = (*gate).clone();
                let config = runner_config.clone();
                let idx = *idx;
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    eprintln!("  [{}] Starting: {}", idx + 1, gate.name);
                    let result = run_gate(&gate, &config);
                    if let Some(callback) = config.on_gate_complete {
                        callback(&result);
                    }
                    eprintln!(
                        "  [{}] Finished: {} - {}",
                        idx + 1,
                        gate.name,
                        result.status.as_str()
                    );
                    let mut locked = results.lock().unwrap();
                    locked.push((idx, result));
                })
            })
            .collect();

        // Wait for all compile gates to complete
        for handle in handles {
            let _ = handle.join();
        }
    }

    // Check if compile phase failed - if so, skip remaining gates
    let compile_failed = {
        let locked = results.lock().unwrap();
        locked.iter().any(|(_, r)| r.status == GateStatus::Fail)
    };

    // Phase 2: Run remaining gates in parallel (if compile passed)
    if compile_failed {
        eprintln!("Phase 2: Skipping remaining gates due to compile failures...");
        // Add skip results for remaining gates
        let mut locked = results.lock().unwrap();
        for (idx, gate) in &other_gates {
            if let Some(callback) = runner_config.on_gate_start {
                callback(&gate.name, *idx, total);
            }
            let result = GateResult::skip(gate, "skipped due to compile failure");
            if let Some(callback) = runner_config.on_gate_complete {
                callback(&result);
            }
            locked.push((*idx, result));
        }
    } else {
        eprintln!(
            "Phase 2: Running remaining {} gates in parallel...",
            other_gates.len()
        );
        let handles: Vec<_> = other_gates
            .iter()
            .map(|(idx, gate)| {
                if let Some(callback) = runner_config.on_gate_start {
                    callback(&gate.name, *idx, total);
                }
                let gate = (*gate).clone();
                let config = runner_config.clone();
                let idx = *idx;
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    eprintln!("  [{}] Starting: {}", idx + 1, gate.name);
                    let result = run_gate(&gate, &config);
                    if let Some(callback) = config.on_gate_complete {
                        callback(&result);
                    }
                    eprintln!(
                        "  [{}] Finished: {} - {}",
                        idx + 1,
                        gate.name,
                        result.status.as_str()
                    );
                    let mut locked = results.lock().unwrap();
                    locked.push((idx, result));
                })
            })
            .collect();

        // Wait for all gates to complete
        for handle in handles {
            let _ = handle.join();
        }
    }

    // Sort results by original gate order
    let mut final_results: Vec<_> = {
        let locked = results.lock().unwrap();
        locked.clone()
    };
    final_results.sort_by_key(|(idx, _)| *idx);
    let results: Vec<GateResult> = final_results.into_iter().map(|(_, r)| r).collect();

    // Verify we have all gates
    assert_eq!(
        results.len(),
        total,
        "parallel run must produce same number of results"
    );

    GateReport::new_with_gate_configs(runner_config.mode, results, gates)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PARALLEL_START_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static PARALLEL_COMPLETE_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn parallel_start_callback(_name: &str, _idx: usize, _total: usize) {
        PARALLEL_START_CALLBACK_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    fn parallel_complete_callback(_result: &GateResult) {
        PARALLEL_COMPLETE_CALLBACK_COUNT.fetch_add(1, Ordering::SeqCst);
    }

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
        assert_eq!(gates.len(), 15, "Expected 15 default gates");
    }

    #[test]
    fn test_default_gates_skip_in_quick() {
        let gates = default_gates();
        let quick_skip: Vec<_> = gates.iter().filter(|g| g.skip_in_quick).collect();
        assert_eq!(
            quick_skip.len(),
            6,
            "Expected 6 gates to skip in quick mode"
        );

        let names: Vec<_> = quick_skip.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"DB stress suite"));
        assert!(names.contains(&"E2E full matrix"));
        assert!(names.contains(&"E2E dual-mode"));
        assert!(names.contains(&"E2E mode matrix"));
        assert!(names.contains(&"E2E security/privacy"));
        assert!(names.contains(&"E2E TUI accessibility"));
    }

    #[test]
    fn test_gate_config_builder() {
        let gate = GateConfig::new("Test gate", GateCategory::Quality, ["cargo", "test"])
            .skip_in_quick()
            .parallel_group("group-a")
            .expected_artifacts(["tests/artifacts/test/*", "tests/artifacts/test_logs/*"]);

        assert_eq!(gate.name, "Test gate");
        assert!(gate.skip_in_quick);
        assert_eq!(gate.parallel_group, Some("group-a".to_string()));
        assert_eq!(gate.expected_artifacts.len(), 2);
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
                error: None,
            },
            GateResult {
                name: "Gate 2".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Fail,
                elapsed_seconds: 5,
                command: "test".to_string(),
                stderr_tail: Some("error".to_string()),
                error: Some(GateError::simple("error")),
            },
            GateResult {
                name: "Gate 3".to_string(),
                category: GateCategory::Security,
                status: GateStatus::Skip,
                elapsed_seconds: 0,
                command: "--quick".to_string(),
                stderr_tail: None,
                error: None,
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
                error: None,
            },
            GateResult {
                name: "Quality 2".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 5,
                command: "test".to_string(),
                stderr_tail: None,
                error: None,
            },
            GateResult {
                name: "Quality 3".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Skip,
                elapsed_seconds: 0,
                command: "--quick".to_string(),
                stderr_tail: None,
                error: None,
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
            error: None,
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
            error: Some(GateError::simple("compilation error")),
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
            error: None,
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
            error: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        let json = report.to_json().expect("serialization should succeed");

        assert!(json.contains("\"schema_version\": \"am_ci_gate_report.v1\""));
        assert!(json.contains("\"decision\": \"go\""));
    }

    #[test]
    fn test_gate_report_includes_execution_log_and_artifact_links() {
        let gate_configs = vec![
            GateConfig::new(
                "E2E dual-mode",
                GateCategory::Quality,
                ["bash", "scripts/e2e_dual_mode.sh"],
            )
            .expected_artifact("tests/artifacts/dual_mode/*"),
            GateConfig::new("Clippy", GateCategory::Quality, ["cargo", "clippy"]),
        ];
        let results = vec![
            GateResult {
                name: "E2E dual-mode".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 14,
                command: "bash scripts/e2e_dual_mode.sh".to_string(),
                stderr_tail: None,
                error: None,
            },
            GateResult {
                name: "Clippy".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Fail,
                elapsed_seconds: 5,
                command: "cargo clippy".to_string(),
                stderr_tail: Some("timeout after 600s".to_string()),
                error: Some(GateError::simple_with_category(
                    "timeout after 600s",
                    Some("timeout"),
                )),
            },
        ];

        let report = GateReport::new_with_gate_configs(RunMode::Full, results, &gate_configs);

        let quality = report
            .category_breakdown
            .get(&GateCategory::Quality)
            .expect("quality breakdown");
        assert_eq!(quality.total, 2);
        assert_eq!(quality.pass, 1);
        assert_eq!(quality.fail, 1);
        assert_eq!(quality.skip, 0);

        let e2e_links = report
            .artifact_links
            .get("E2E dual-mode")
            .expect("dual-mode artifact links");
        assert_eq!(e2e_links, &vec!["tests/artifacts/dual_mode/*".to_string()]);

        assert_eq!(report.execution_log.len(), 2);
        assert_eq!(report.execution_log[0].normalized_exit_code, 0);
        assert_eq!(report.execution_log[1].normalized_exit_code, 124);
        assert_eq!(
            report.execution_log[1].error_category.as_deref(),
            Some("timeout")
        );
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
    fn test_run_gate_enforces_timeout_without_stderr_output() {
        let gate = GateConfig::new(
            "Timeout test",
            GateCategory::Quality,
            ["bash", "-c", "sleep 2"],
        );
        let config = GateRunnerConfig::default().timeout_secs(1);

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        assert!(
            result
                .stderr_tail
                .as_deref()
                .is_some_and(|tail| tail.contains("timeout")),
            "expected timeout diagnostic, got {:?}",
            result.stderr_tail
        );
        assert_eq!(
            result
                .error
                .as_ref()
                .and_then(|error| error.error_category.as_deref()),
            Some("timeout")
        );
    }

    #[test]
    fn test_run_gate_fails_when_expected_artifact_missing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let gate = GateConfig::new("Artifact gate", GateCategory::Quality, ["true"])
            .expected_artifact("artifacts/missing.json");
        let config = GateRunnerConfig::new(temp_dir.path());

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        let error = result.error.expect("artifact failure should include error");
        assert_eq!(error.error_category.as_deref(), Some("artifact_missing"));
        assert!(
            error
                .affected_files
                .iter()
                .any(|path| path == "artifacts/missing.json")
        );
    }

    #[test]
    fn test_run_gate_reports_partial_artifact_outputs() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(temp_dir.path().join("artifacts")).expect("create artifact dir");
        std::fs::write(temp_dir.path().join("artifacts/found.json"), "{}").expect("write artifact");

        let gate = GateConfig::new("Artifact partial gate", GateCategory::Quality, ["true"])
            .expected_artifacts(["artifacts/found.json", "artifacts/missing.json"]);
        let config = GateRunnerConfig::new(temp_dir.path());

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        let error = result
            .error
            .expect("partial artifact failure should include error");
        assert_eq!(error.error_category.as_deref(), Some("artifact_partial"));
        assert!(
            error.stderr_tail.contains("found artifacts"),
            "partial failure should include found artifact diagnostics"
        );
    }

    #[test]
    fn test_run_gate_empty_command() {
        let gate = GateConfig {
            name: "Empty".to_string(),
            category: GateCategory::Quality,
            command: vec![],
            skip_in_quick: false,
            parallel_group: None,
            expected_artifacts: Vec::new(),
        };
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        assert!(
            result
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("empty command")
        );
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

    // ── JSON Report Generator Tests ──────────────────────────────────────────

    #[test]
    fn test_gate_report_total_elapsed_seconds() {
        let results = vec![
            GateResult {
                name: "Gate 1".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 10,
                command: "test".to_string(),
                stderr_tail: None,
                error: None,
            },
            GateResult {
                name: "Gate 2".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 25,
                command: "test".to_string(),
                stderr_tail: None,
                error: None,
            },
        ];

        let report = GateReport::new(RunMode::Full, results);
        assert_eq!(report.total_elapsed_seconds, 35);
    }

    #[test]
    fn test_gate_report_failed_gates() {
        let results = vec![
            GateResult {
                name: "Pass Gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 10,
                command: "test".to_string(),
                stderr_tail: None,
                error: None,
            },
            GateResult {
                name: "Fail Gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Fail,
                elapsed_seconds: 5,
                command: "test".to_string(),
                stderr_tail: Some("error".to_string()),
                error: Some(GateError::simple("error")),
            },
        ];

        let report = GateReport::new(RunMode::Full, results);
        let failed = report.failed_gates();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "Fail Gate");
    }

    #[test]
    fn test_gate_report_skipped_gates() {
        let results = vec![
            GateResult {
                name: "Pass Gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 10,
                command: "test".to_string(),
                stderr_tail: None,
                error: None,
            },
            GateResult {
                name: "Skip Gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Skip,
                elapsed_seconds: 0,
                command: "--quick".to_string(),
                stderr_tail: None,
                error: None,
            },
        ];

        let report = GateReport::new(RunMode::Quick, results);
        let skipped = report.skipped_gates();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "Skip Gate");
    }

    #[test]
    fn test_gate_report_failure_summary_no_failures() {
        let results = vec![GateResult {
            name: "Pass Gate".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 10,
            command: "test".to_string(),
            stderr_tail: None,
            error: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        assert_eq!(report.failure_summary(), "No failures.");
    }

    #[test]
    fn test_gate_report_failure_summary_with_failures() {
        let results = vec![GateResult {
            name: "Clippy".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Fail,
            elapsed_seconds: 5,
            command: "cargo clippy".to_string(),
            stderr_tail: Some("error: unused variable\n  --> src/main.rs:5".to_string()),
            error: Some(GateError::from_stderr(
                "error: unused variable\n  --> src/main.rs:5",
            )),
        }];

        let report = GateReport::new(RunMode::Full, results);
        let summary = report.failure_summary();
        assert!(summary.contains("1 gate(s) failed:"));
        assert!(summary.contains("Clippy"));
        assert!(summary.contains("unused variable"));
    }

    #[test]
    fn test_gate_report_roundtrip_json() {
        let results = vec![GateResult {
            name: "Test Gate".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 15,
            command: "cargo test".to_string(),
            stderr_tail: None,
            error: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        let json = report.to_json().expect("serialization should work");
        let parsed = GateReport::from_json(&json).expect("deserialization should work");

        assert_eq!(parsed.schema_version, report.schema_version);
        assert_eq!(parsed.mode, report.mode);
        assert_eq!(parsed.decision, report.decision);
        assert_eq!(parsed.total_elapsed_seconds, report.total_elapsed_seconds);
        assert_eq!(parsed.gates.len(), 1);
        assert_eq!(parsed.gates[0].name, "Test Gate");
    }

    #[test]
    fn test_gate_report_with_timestamp_is_deterministic() {
        let timestamp = DateTime::parse_from_rfc3339("2026-02-12T08:00:00Z")
            .expect("timestamp parse should succeed")
            .with_timezone(&Utc);

        let report = GateReport::with_timestamp(
            RunMode::Full,
            vec![GateResult {
                name: "Deterministic timestamp gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 1,
                command: "true".to_string(),
                stderr_tail: None,
                error: None,
            }],
            timestamp,
        );

        assert_eq!(report.generated_at, "2026-02-12T08:00:00Z");
    }

    #[test]
    fn test_gate_report_compact_json_has_no_newlines() {
        let report = GateReport::new(
            RunMode::Full,
            vec![GateResult {
                name: "Compact JSON gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Pass,
                elapsed_seconds: 1,
                command: "true".to_string(),
                stderr_tail: None,
                error: None,
            }],
        );

        let compact = report
            .to_json_compact()
            .expect("compact JSON serialization should work");
        assert!(!compact.contains('\n'));
        let parsed: serde_json::Value =
            serde_json::from_str(&compact).expect("compact JSON must be valid");
        assert_eq!(parsed["schema_version"], GATE_REPORT_SCHEMA_VERSION);
    }

    #[test]
    fn test_gate_report_from_json_invalid_payload_errors() {
        let err = GateReport::from_json("{not valid json").expect_err("must fail");
        assert!(err.is_syntax() || err.is_data() || err.is_eof());
    }

    #[test]
    fn test_gate_report_from_file_invalid_data_kind() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file should create");
        std::fs::write(tmp.path(), b"not-json").expect("write should succeed");

        let err = GateReport::from_file(tmp.path()).expect_err("must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_gate_report_failure_summary_uses_first_stderr_line_only() {
        let report = GateReport::new(
            RunMode::Full,
            vec![GateResult {
                name: "Failing gate".to_string(),
                category: GateCategory::Quality,
                status: GateStatus::Fail,
                elapsed_seconds: 1,
                command: "false".to_string(),
                stderr_tail: Some("first line\nsecond line\nthird line".to_string()),
                error: Some(GateError::simple("first line")),
            }],
        );

        let summary = report.failure_summary();
        assert!(summary.contains("first line"));
        assert!(!summary.contains("second line"));
    }

    #[test]
    fn test_gate_report_write_and_read_file() {
        use std::fs;

        let results = vec![GateResult {
            name: "File IO Test".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 5,
            command: "test".to_string(),
            stderr_tail: None,
            error: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        let temp_dir = tempfile::tempdir().expect("tempdir should create");
        let temp_path = temp_dir.path().join("gate_report_test.json");

        // Write to file
        report.write_to_file(&temp_path).expect("write should work");

        // Read back
        let loaded = GateReport::from_file(&temp_path).expect("read should work");

        assert_eq!(loaded.schema_version, report.schema_version);
        assert_eq!(loaded.gates[0].name, "File IO Test");

        // Cleanup
        let _ = fs::remove_file(&temp_path);
    }

    // ── GateError Tests (T1.6) ────────────────────────────────────────────────

    #[test]
    fn test_gate_error_from_stderr_compiler_error() {
        let stderr = r#"error[E0425]: cannot find value `foo` in this scope
   --> src/main.rs:5:9
    |
5   |     let x = foo;
    |             ^^^ not found in this scope

error[E0308]: mismatched types
   --> src/lib.rs:10:5
    |
10  |     42
    |     ^^ expected `&str`, found integer

error: aborting due to 2 previous errors"#;

        let gate_error = GateError::from_stderr(stderr);

        assert_eq!(gate_error.error_category, Some("compiler".to_string()));
        assert_eq!(gate_error.error_count, Some(2));
        assert!(
            gate_error
                .error_summary
                .as_ref()
                .unwrap()
                .contains("2 compiler error(s)")
        );
        assert!(
            gate_error
                .error_summary
                .as_ref()
                .unwrap()
                .contains("cannot find value")
        );
        assert!(
            gate_error
                .affected_files
                .contains(&"src/main.rs".to_string())
        );
        assert!(
            gate_error
                .affected_files
                .contains(&"src/lib.rs".to_string())
        );
    }

    #[test]
    fn test_gate_error_from_stderr_test_failure() {
        let stderr = r#"running 3 tests
test tests::test_pass ... ok
test tests::test_fail ... FAILED
test tests::test_another ... ok

failures:

---- tests::test_fail ----
thread 'tests::test_fail' panicked at src/lib.rs:42:5:
assertion failed: `(left == right)`
  left: `1`,
 right: `2`

failures:
    tests::test_fail

test result: FAILED. 2 passed; 1 failed; 0 ignored"#;

        let gate_error = GateError::from_stderr(stderr);

        assert_eq!(gate_error.error_category, Some("test".to_string()));
        assert_eq!(gate_error.error_count, Some(2)); // test_fail header + panicked
        assert!(
            gate_error
                .error_summary
                .as_ref()
                .unwrap()
                .contains("test failure(s)")
        );
    }

    #[test]
    fn test_gate_error_from_stderr_clippy_warning() {
        let stderr = r#"warning: unused variable: `x`
 --> src/main.rs:3:9
  |
3 |     let x = 5;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`
  |
  = note: `#[warn(unused_variables)]` on by default

warning: unused variable: `y`
 --> src/main.rs:4:9
  |
4 |     let y = 10;
  |         ^ help: if this is intentional, prefix it with an underscore: `_y`

warning: `myproject` (bin "myproject") generated 2 warnings"#;

        let gate_error = GateError::from_stderr(stderr);

        assert_eq!(gate_error.error_category, Some("clippy".to_string()));
        // At least 2 warnings
        assert!(gate_error.error_count.unwrap() >= 2);
        assert!(
            gate_error
                .error_summary
                .as_ref()
                .unwrap()
                .contains("clippy warning(s)")
        );
    }

    #[test]
    fn test_gate_error_from_stderr_empty() {
        let gate_error = GateError::from_stderr("");

        assert!(gate_error.stderr_tail.is_empty());
        assert!(gate_error.error_category.is_none());
        assert!(gate_error.error_count.is_none());
        assert!(gate_error.error_summary.is_none());
        assert!(gate_error.affected_files.is_empty());
    }

    #[test]
    fn test_gate_error_from_stderr_unknown() {
        let stderr = "Some random error message\nAnother line";

        let gate_error = GateError::from_stderr(stderr);

        assert_eq!(gate_error.error_category, Some("unknown".to_string()));
        assert!(gate_error.error_count.is_none());
        assert!(
            gate_error
                .error_summary
                .as_ref()
                .unwrap()
                .contains("Some random error message")
        );
    }

    #[test]
    fn test_gate_error_stderr_truncation() {
        // Create stderr with more than 100 lines
        let long_stderr: String = (0..150)
            .map(|i| format!("line {}: error message", i))
            .collect::<Vec<_>>()
            .join("\n");

        let gate_error = GateError::from_stderr(&long_stderr);

        // Should only have 100 lines (the last 100)
        let line_count = gate_error.stderr_tail.lines().count();
        assert_eq!(line_count, 100);
        // Should contain the last line (line 149)
        assert!(gate_error.stderr_tail.contains("line 149"));
        // Should NOT contain the first line (line 0)
        assert!(!gate_error.stderr_tail.contains("line 0:"));
    }

    #[test]
    fn test_gate_error_simple() {
        let gate_error = GateError::simple("spawn failed: command not found");

        assert_eq!(gate_error.stderr_tail, "spawn failed: command not found");
        assert_eq!(
            gate_error.error_summary,
            Some("spawn failed: command not found".to_string())
        );
        assert!(gate_error.error_category.is_none());
        assert!(gate_error.error_count.is_none());
    }

    #[test]
    fn test_gate_error_extract_affected_files() {
        let stderr = r#"error[E0425]: cannot find value `foo`
   --> src/main.rs:5:9
error[E0425]: another error
   --> src/lib.rs:10:5
   --> tests/integration.rs:42:13"#;

        let gate_error = GateError::from_stderr(stderr);

        assert_eq!(gate_error.affected_files.len(), 3);
        assert!(
            gate_error
                .affected_files
                .contains(&"src/main.rs".to_string())
        );
        assert!(
            gate_error
                .affected_files
                .contains(&"src/lib.rs".to_string())
        );
        assert!(
            gate_error
                .affected_files
                .contains(&"tests/integration.rs".to_string())
        );
    }

    #[test]
    fn test_gate_result_fail_creates_structured_error() {
        let gate = GateConfig::new("Compile", GateCategory::Quality, ["cargo", "build"]);
        let stderr = "error[E0425]: cannot find value `x`\n   --> src/main.rs:5:9";

        let result = GateResult::fail(&gate, Duration::from_secs(5), Some(stderr.to_string()));

        assert!(result.error.is_some());
        let error = result.error.unwrap();
        assert_eq!(error.error_category, Some("compiler".to_string()));
        assert_eq!(error.error_count, Some(1));
        assert!(error.affected_files.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn test_gate_result_pass_has_no_error() {
        let gate = GateConfig::new("Build", GateCategory::Quality, ["cargo", "build"]);

        let result = GateResult::pass(&gate, Duration::from_secs(10));

        assert!(result.error.is_none());
        assert!(result.stderr_tail.is_none());
    }

    #[test]
    fn test_run_gate_produces_structured_error_on_failure() {
        let gate = GateConfig::new(
            "Stderr structured test",
            GateCategory::Quality,
            [
                "bash",
                "-c",
                "echo 'error[E0425]: cannot find value' >&2 && echo '   --> src/test.rs:10:5' >&2 && exit 1",
            ],
        );
        let config = GateRunnerConfig::default();

        let result = run_gate(&gate, &config);

        assert_eq!(result.status, GateStatus::Fail);
        assert!(result.error.is_some());
        let error = result.error.unwrap();
        assert_eq!(error.error_category, Some("compiler".to_string()));
        assert!(error.affected_files.contains(&"src/test.rs".to_string()));
    }

    #[test]
    fn test_gate_error_appears_in_json_for_failing_gate() {
        let results = vec![GateResult {
            name: "Compile".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Fail,
            elapsed_seconds: 5,
            command: "cargo build".to_string(),
            stderr_tail: Some("error[E0425]: cannot find value".to_string()),
            error: Some(GateError {
                stderr_tail: "error[E0425]: cannot find value".to_string(),
                error_count: Some(1),
                error_summary: Some("1 compiler error(s): cannot find value".to_string()),
                affected_files: vec!["src/main.rs".to_string()],
                error_category: Some("compiler".to_string()),
            }),
        }];

        let report = GateReport::new(RunMode::Full, results);
        let json = report.to_json().expect("serialization should work");

        assert!(json.contains("\"error_category\": \"compiler\""));
        assert!(json.contains("\"error_count\": 1"));
        assert!(json.contains("\"affected_files\""));
        assert!(json.contains("src/main.rs"));
    }

    #[test]
    fn test_gate_error_not_present_for_passing_gate() {
        let results = vec![GateResult {
            name: "Build".to_string(),
            category: GateCategory::Quality,
            status: GateStatus::Pass,
            elapsed_seconds: 10,
            command: "cargo build".to_string(),
            stderr_tail: None,
            error: None,
        }];

        let report = GateReport::new(RunMode::Full, results);
        let json = report.to_json().expect("serialization should work");

        // error field should not appear in JSON for passing gates
        assert!(!json.contains("\"error\":"));
        assert!(!json.contains("\"error_category\""));
    }

    // ── Parallel Execution Tests (T1.5) ───────────────────────────────────────

    #[test]
    fn test_parallel_execution_produces_same_results_as_sequential() {
        // Create simple test gates that will pass
        let gates = vec![
            GateConfig::new("Pass 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Pass 2", GateCategory::Quality, ["true"]),
            GateConfig::new("Pass 3", GateCategory::Performance, ["true"]),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        // Run sequentially
        let seq_report = run_gates(&gates, &config);

        // Run in parallel
        let par_report = run_gates_parallel(&gates, &config);

        // Same number of gates
        assert_eq!(seq_report.gates.len(), par_report.gates.len());

        // Same overall decision
        assert_eq!(seq_report.decision, par_report.decision);

        // Same summary counts
        assert_eq!(seq_report.summary.total, par_report.summary.total);
        assert_eq!(seq_report.summary.pass, par_report.summary.pass);
        assert_eq!(seq_report.summary.fail, par_report.summary.fail);
        assert_eq!(seq_report.summary.skip, par_report.summary.skip);

        // Same gate names and statuses (order should be preserved)
        for (seq_gate, par_gate) in seq_report.gates.iter().zip(par_report.gates.iter()) {
            assert_eq!(seq_gate.name, par_gate.name);
            assert_eq!(seq_gate.status, par_gate.status);
        }
    }

    #[test]
    fn test_parallel_execution_handles_failures() {
        // Create gates with one failure
        let gates = vec![
            GateConfig::new("Pass 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Fail gate", GateCategory::Quality, ["false"]),
            GateConfig::new("Pass 2", GateCategory::Performance, ["true"]),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let report = run_gates_parallel(&gates, &config);

        // Should report all gates
        assert_eq!(report.gates.len(), 3);

        // Should have the correct decision
        assert_eq!(report.decision, Decision::NoGo);

        // Should have correct counts
        assert_eq!(report.summary.total, 3);
        assert!(report.summary.fail >= 1);
    }

    #[test]
    fn test_parallel_preserves_gate_order_in_results() {
        // Create gates with distinct names
        let gates = vec![
            GateConfig::new("Alpha", GateCategory::Quality, ["true"]),
            GateConfig::new("Beta", GateCategory::Quality, ["true"]),
            GateConfig::new("Gamma", GateCategory::Performance, ["true"]),
            GateConfig::new("Delta", GateCategory::Security, ["true"]),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let report = run_gates_parallel(&gates, &config);

        // Results should be in the same order as input gates
        assert_eq!(report.gates[0].name, "Alpha");
        assert_eq!(report.gates[1].name, "Beta");
        assert_eq!(report.gates[2].name, "Gamma");
        assert_eq!(report.gates[3].name, "Delta");
    }

    #[test]
    fn test_parallel_captures_stderr_per_gate() {
        // Create a failing gate that produces stderr
        let gates = vec![GateConfig::new(
            "Stderr producer",
            GateCategory::Quality,
            [
                "bash",
                "-c",
                "echo 'unique_error_marker_12345' >&2 && exit 1",
            ],
        )];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let report = run_gates_parallel(&gates, &config);

        assert_eq!(report.gates.len(), 1);
        assert_eq!(report.gates[0].status, GateStatus::Fail);
        assert!(report.gates[0].stderr_tail.is_some());
        assert!(
            report.gates[0]
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("unique_error_marker_12345")
        );
    }

    #[test]
    fn test_parallel_no_interleaved_output() {
        // Create multiple gates that each produce distinct output
        let gates = vec![
            GateConfig::new(
                "Gate A",
                GateCategory::Quality,
                ["bash", "-c", "echo 'GATE_A_OUTPUT' >&2 && exit 1"],
            ),
            GateConfig::new(
                "Gate B",
                GateCategory::Performance,
                ["bash", "-c", "echo 'GATE_B_OUTPUT' >&2 && exit 1"],
            ),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let report = run_gates_parallel(&gates, &config);

        // Each gate should have only its own output, not interleaved
        let gate_a = &report.gates[0];
        let gate_b = &report.gates[1];

        assert!(
            gate_a
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("GATE_A_OUTPUT")
        );
        assert!(
            !gate_a
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("GATE_B_OUTPUT")
        );

        assert!(
            gate_b
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("GATE_B_OUTPUT")
        );
        assert!(
            !gate_b
                .stderr_tail
                .as_ref()
                .unwrap()
                .contains("GATE_A_OUTPUT")
        );
    }

    #[test]
    fn test_parallel_respects_quick_mode() {
        let gates = vec![
            GateConfig::new("Normal gate", GateCategory::Quality, ["true"]),
            GateConfig::new("E2E gate", GateCategory::Quality, ["true"]).skip_in_quick(),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Quick);

        let report = run_gates_parallel(&gates, &config);

        // Normal gate should pass
        assert_eq!(report.gates[0].status, GateStatus::Pass);
        // E2E gate should be skipped
        assert_eq!(report.gates[1].status, GateStatus::Skip);
    }

    #[test]
    fn test_parallel_report_json_structure_matches_sequential() {
        let gates = vec![
            GateConfig::new("Test 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Test 2", GateCategory::Performance, ["true"]),
        ];
        let config = GateRunnerConfig::default().mode(RunMode::Full);

        let seq_report = run_gates(&gates, &config);
        let par_report = run_gates_parallel(&gates, &config);

        let seq_json = seq_report.to_json().expect("seq serialization");
        let par_json = par_report.to_json().expect("par serialization");

        // Both should have the same schema version
        assert!(seq_json.contains("\"schema_version\": \"am_ci_gate_report.v1\""));
        assert!(par_json.contains("\"schema_version\": \"am_ci_gate_report.v1\""));

        // Both should have same decision
        assert!(seq_json.contains("\"decision\": \"go\""));
        assert!(par_json.contains("\"decision\": \"go\""));

        // Both should have same gate names
        assert!(seq_json.contains("\"name\": \"Test 1\""));
        assert!(par_json.contains("\"name\": \"Test 1\""));
        assert!(seq_json.contains("\"name\": \"Test 2\""));
        assert!(par_json.contains("\"name\": \"Test 2\""));
    }

    #[test]
    fn test_parallel_callbacks_are_invoked_for_each_gate() {
        PARALLEL_START_CALLBACK_COUNT.store(0, Ordering::SeqCst);
        PARALLEL_COMPLETE_CALLBACK_COUNT.store(0, Ordering::SeqCst);

        let gates = vec![
            GateConfig::new("Callback 1", GateCategory::Quality, ["true"]),
            GateConfig::new("Callback 2", GateCategory::Performance, ["true"]),
            GateConfig::new("Callback 3", GateCategory::Security, ["true"]),
        ];
        let mut config = GateRunnerConfig::default().mode(RunMode::Full);
        config.on_gate_start = Some(parallel_start_callback);
        config.on_gate_complete = Some(parallel_complete_callback);

        let report = run_gates_parallel(&gates, &config);
        assert_eq!(report.gates.len(), gates.len());
        assert_eq!(
            PARALLEL_START_CALLBACK_COUNT.load(Ordering::SeqCst),
            gates.len()
        );
        assert_eq!(
            PARALLEL_COMPLETE_CALLBACK_COUNT.load(Ordering::SeqCst),
            gates.len()
        );
    }
}
