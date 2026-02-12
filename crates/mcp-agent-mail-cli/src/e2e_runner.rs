//! Native E2E Suite Registry and Runner
//!
//! This module implements the native E2E test runner for `am e2e` command,
//! providing suite discovery, execution, and reporting.
//!
//! Implements: `br-8zmc` (T9.3)
//!
//! # Commands
//!
//! - `am e2e list` - List available test suites
//! - `am e2e run [suites...]` - Run specified suites (or all if none specified)
//! - `am e2e run --include <pattern>` - Run suites matching pattern
//! - `am e2e run --exclude <pattern>` - Skip suites matching pattern
//!
//! # Suite Discovery
//!
//! Suites are discovered from `tests/e2e/test_*.sh` files. Each file is a suite.
//! Suite names are derived from filenames: `test_foo.sh` → `foo`.
//!
//! # Execution Model
//!
//! Each suite runs in a subprocess with isolated environment. The runner captures:
//! - Exit code (0 = pass, non-zero = fail)
//! - stdout/stderr output
//! - Execution timing
//!
//! Results are aggregated into JSON reports compatible with `e2e_artifacts`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// Suite Registry
// ──────────────────────────────────────────────────────────────────────────────

/// A registered E2E test suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suite {
    /// Suite name (e.g., "guard", "http", "stdio").
    pub name: String,
    /// Path to the test script.
    pub script_path: PathBuf,
    /// Optional description extracted from script header.
    pub description: Option<String>,
    /// Tags/labels extracted from script (e.g., "slow", "flaky").
    pub tags: Vec<String>,
    /// Estimated duration category.
    pub duration_class: DurationClass,
}

/// Duration classification for suites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DurationClass {
    /// Fast suite (< 10s).
    Fast,
    /// Normal suite (10-60s).
    #[default]
    Normal,
    /// Slow suite (> 60s).
    Slow,
}

impl DurationClass {
    /// Returns the string representation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Normal => "normal",
            Self::Slow => "slow",
        }
    }
}

/// Suite registry for discovering and managing test suites.
#[derive(Debug)]
pub struct SuiteRegistry {
    /// Project root directory.
    project_root: PathBuf,
    /// Discovered suites (name → Suite).
    suites: HashMap<String, Suite>,
}

impl SuiteRegistry {
    /// Creates a new registry and discovers suites.
    pub fn new(project_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let project_root = project_root.as_ref().to_path_buf();
        let mut registry = Self {
            project_root,
            suites: HashMap::new(),
        };
        registry.discover_suites()?;
        Ok(registry)
    }

    /// Discovers suites from tests/e2e/test_*.sh files.
    fn discover_suites(&mut self) -> std::io::Result<()> {
        let e2e_dir = self.project_root.join("tests/e2e");
        if !e2e_dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&e2e_dir)? {
            let entry = entry?;
            let path = entry.path();

            // Only consider test_*.sh files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("test_") && name.ends_with(".sh") {
                    let suite_name = name
                        .strip_prefix("test_")
                        .unwrap()
                        .strip_suffix(".sh")
                        .unwrap()
                        .to_string();

                    let (description, tags) = Self::extract_metadata(&path);
                    let duration_class = Self::classify_duration(&suite_name, &tags);

                    self.suites.insert(
                        suite_name.clone(),
                        Suite {
                            name: suite_name,
                            script_path: path,
                            description,
                            tags,
                            duration_class,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    /// Extracts description and tags from script header comments.
    fn extract_metadata(path: &Path) -> (Option<String>, Vec<String>) {
        let mut description = None;
        let mut tags = Vec::new();

        if let Ok(file) = fs::File::open(path) {
            let reader = BufReader::new(file);
            for line in reader.lines().take(20).map_while(Result::ok) {
                let line = line.trim();

                // Look for description in header comments
                if line.starts_with("# ") && description.is_none() {
                    let content = line.strip_prefix("# ").unwrap_or("");
                    // Skip shebang and common headers
                    if !content.starts_with("!") && !content.contains("e2e_lib.sh") {
                        description = Some(content.to_string());
                    }
                }

                // Look for tags (e.g., "# @tags: slow, flaky")
                if let Some(tag_line) = line.strip_prefix("# @tags:") {
                    tags = tag_line
                        .split(',')
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
            }
        }

        (description, tags)
    }

    /// Classifies suite duration based on name and tags.
    fn classify_duration(name: &str, tags: &[String]) -> DurationClass {
        // Explicit slow tag
        if tags.iter().any(|t| t == "slow") {
            return DurationClass::Slow;
        }

        // Known slow suites
        const SLOW_SUITES: &[&str] = &[
            "concurrent",
            "crash_restart",
            "fault_injection",
            "large_inputs",
            "db_corruption",
            "db_migration",
        ];
        for prefix in SLOW_SUITES {
            if name.contains(prefix) {
                return DurationClass::Slow;
            }
        }

        // Known fast suites
        const FAST_SUITES: &[&str] = &["cli", "archive", "console"];
        for prefix in FAST_SUITES {
            if name.contains(prefix) {
                return DurationClass::Fast;
            }
        }

        DurationClass::Normal
    }

    /// Returns all suite names in deterministic order.
    #[must_use]
    pub fn suite_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.suites.keys().cloned().collect();
        names.sort();
        names
    }

    /// Returns all suites in deterministic order.
    #[must_use]
    pub fn suites(&self) -> Vec<&Suite> {
        let mut suites: Vec<_> = self.suites.values().collect();
        suites.sort_by(|a, b| a.name.cmp(&b.name));
        suites
    }

    /// Gets a suite by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Suite> {
        self.suites.get(name)
    }

    /// Returns the number of registered suites.
    #[must_use]
    pub fn len(&self) -> usize {
        self.suites.len()
    }

    /// Returns true if no suites are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.suites.is_empty()
    }

    /// Filters suites by include/exclude patterns.
    pub fn filter(&self, include: Option<&[String]>, exclude: Option<&[String]>) -> Vec<&Suite> {
        self.suites()
            .into_iter()
            .filter(|suite| {
                // If include patterns specified, suite must match at least one
                let included = include.map_or(true, |patterns| {
                    patterns
                        .iter()
                        .any(|p| Self::matches_pattern(&suite.name, p))
                });

                // If exclude patterns specified, suite must not match any
                let excluded = exclude.map_or(false, |patterns| {
                    patterns
                        .iter()
                        .any(|p| Self::matches_pattern(&suite.name, p))
                });

                included && !excluded
            })
            .collect()
    }

    /// Simple glob-like pattern matching.
    fn matches_pattern(name: &str, pattern: &str) -> bool {
        if pattern.contains('*') {
            // Simple wildcard matching
            let parts: Vec<&str> = pattern.split('*').collect();
            if parts.len() == 2 {
                let (prefix, suffix) = (parts[0], parts[1]);
                return name.starts_with(prefix) && name.ends_with(suffix);
            }
        }
        // Exact or substring match
        name == pattern || name.contains(pattern)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Suite Execution
// ──────────────────────────────────────────────────────────────────────────────

/// Result of running a single suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResult {
    /// Suite name.
    pub name: String,
    /// Whether the suite passed.
    pub passed: bool,
    /// Exit code from the test script.
    pub exit_code: i32,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Captured stdout (truncated if too long).
    pub stdout: String,
    /// Captured stderr (truncated if too long).
    pub stderr: String,
    /// Number of assertions passed (parsed from output).
    pub assertions_passed: u32,
    /// Number of assertions failed (parsed from output).
    pub assertions_failed: u32,
    /// Number of assertions skipped (parsed from output).
    pub assertions_skipped: u32,
    /// Start timestamp (RFC3339).
    pub started_at: String,
    /// End timestamp (RFC3339).
    pub ended_at: String,
}

/// Configuration for running suites.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Project root directory.
    pub project_root: PathBuf,
    /// Artifact output directory (optional).
    pub artifact_dir: Option<PathBuf>,
    /// Maximum output capture per suite (bytes).
    pub max_output_bytes: usize,
    /// Timeout per suite (None = no timeout).
    pub timeout: Option<Duration>,
    /// Environment variables to pass.
    pub env: HashMap<String, String>,
    /// Whether to run in parallel.
    pub parallel: bool,
    /// Keep temporary directories.
    pub keep_tmp: bool,
    /// Force rebuild before running.
    pub force_build: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            project_root: PathBuf::from("."),
            artifact_dir: None,
            max_output_bytes: 256 * 1024,            // 256KB
            timeout: Some(Duration::from_secs(600)), // 10 minutes
            env: HashMap::new(),
            parallel: false,
            keep_tmp: false,
            force_build: false,
        }
    }
}

/// E2E test runner.
#[derive(Debug)]
pub struct Runner {
    /// Registry of available suites.
    registry: SuiteRegistry,
    /// Run configuration.
    config: RunConfig,
}

impl Runner {
    /// Creates a new runner.
    pub fn new(project_root: impl AsRef<Path>, config: RunConfig) -> std::io::Result<Self> {
        let registry = SuiteRegistry::new(project_root)?;
        Ok(Self { registry, config })
    }

    /// Returns the suite registry.
    #[must_use]
    pub fn registry(&self) -> &SuiteRegistry {
        &self.registry
    }

    /// Runs the specified suites (or all if empty).
    pub fn run(&self, suite_names: &[String]) -> RunReport {
        let run_started = Utc::now();
        let start_instant = Instant::now();

        // Determine which suites to run
        let suites: Vec<&Suite> = if suite_names.is_empty() {
            self.registry.suites()
        } else {
            suite_names
                .iter()
                .filter_map(|name| self.registry.get(name))
                .collect()
        };

        let mut results = Vec::with_capacity(suites.len());
        let mut passed = 0;
        let mut failed = 0;

        for suite in &suites {
            let result = self.run_suite(suite);
            if result.passed {
                passed += 1;
            } else {
                failed += 1;
            }
            results.push(result);
        }

        let run_ended = Utc::now();
        let elapsed = start_instant.elapsed();

        RunReport {
            total: suites.len() as u32,
            passed,
            failed,
            skipped: 0,
            duration_ms: elapsed.as_millis() as u64,
            started_at: run_started.to_rfc3339(),
            ended_at: run_ended.to_rfc3339(),
            results,
        }
    }

    /// Runs suites with include/exclude filtering.
    pub fn run_filtered(
        &self,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> RunReport {
        let suites = self.registry.filter(include, exclude);
        let suite_names: Vec<String> = suites.iter().map(|s| s.name.clone()).collect();
        self.run(&suite_names)
    }

    /// Runs a single suite.
    fn run_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        // Build the command
        let mut cmd = Command::new("bash");
        cmd.arg(&suite.script_path);
        cmd.current_dir(&self.config.project_root);

        // Set environment
        cmd.env("E2E_PROJECT_ROOT", &self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }

        // Capture output
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Execute
        let output = cmd.output();
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match output {
            Ok(output) => {
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let stderr = Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let exit_code = output.status.code().unwrap_or(-1);
                let passed = output.status.success();

                // Parse assertion counts from output
                let (assertions_passed, assertions_failed, assertions_skipped) =
                    Self::parse_assertions(&stdout);

                SuiteResult {
                    name: suite.name.clone(),
                    passed,
                    exit_code,
                    duration_ms: elapsed.as_millis() as u64,
                    stdout,
                    stderr,
                    assertions_passed,
                    assertions_failed,
                    assertions_skipped,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                }
            }
            Err(e) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute suite: {e}"),
                assertions_passed: 0,
                assertions_failed: 0,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    /// Truncates output to max bytes.
    fn truncate_output(bytes: &[u8], max_bytes: usize) -> String {
        let s = String::from_utf8_lossy(bytes);
        if s.len() <= max_bytes {
            s.into_owned()
        } else {
            let truncated = &s[..max_bytes];
            format!("{truncated}\n... [output truncated at {max_bytes} bytes]")
        }
    }

    /// Parses assertion counts from test output.
    ///
    /// Looks for patterns like:
    /// - "Pass: 27" or "PASS: 27"
    /// - "Fail: 1" or "FAIL: 1"
    /// - "Skip: 2" or "SKIP: 2"
    fn parse_assertions(output: &str) -> (u32, u32, u32) {
        let mut passed = 0u32;
        let mut failed = 0u32;
        let mut skipped = 0u32;

        // Strip ANSI escape codes
        let ansi_regex = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap_or_else(|_| {
            regex::Regex::new(r"$^").unwrap() // Never-matching fallback
        });

        for line in output.lines() {
            let clean_line = ansi_regex.replace_all(line, "");
            let line_lower = clean_line.to_lowercase();

            // Look for summary line with all counts
            // Format: "Total: 7  Pass: 27  Fail: 1  Skip: 1"
            if line_lower.contains("pass:") || line_lower.contains("fail:") {
                let words: Vec<&str> = clean_line.split_whitespace().collect();
                for (i, word) in words.iter().enumerate() {
                    let word_lower = word.to_lowercase();
                    if word_lower == "pass:" {
                        if let Some(num) = words.get(i + 1) {
                            if let Ok(n) = num.parse::<u32>() {
                                passed = n;
                            }
                        }
                    } else if word_lower == "fail:" {
                        if let Some(num) = words.get(i + 1) {
                            if let Ok(n) = num.parse::<u32>() {
                                failed = n;
                            }
                        }
                    } else if word_lower == "skip:" {
                        if let Some(num) = words.get(i + 1) {
                            if let Ok(n) = num.parse::<u32>() {
                                skipped = n;
                            }
                        }
                    }
                }
            }
        }

        (passed, failed, skipped)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Run Report
// ──────────────────────────────────────────────────────────────────────────────

/// Summary report from running suites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Total number of suites run.
    pub total: u32,
    /// Number of suites that passed.
    pub passed: u32,
    /// Number of suites that failed.
    pub failed: u32,
    /// Number of suites skipped.
    pub skipped: u32,
    /// Total duration in milliseconds.
    pub duration_ms: u64,
    /// Start timestamp (RFC3339).
    pub started_at: String,
    /// End timestamp (RFC3339).
    pub ended_at: String,
    /// Individual suite results.
    pub results: Vec<SuiteResult>,
}

impl RunReport {
    /// Returns true if all suites passed.
    #[must_use]
    pub fn success(&self) -> bool {
        self.failed == 0
    }

    /// Returns the exit code (0 = success, 1 = failures).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.success() { 0 } else { 1 }
    }

    /// Formats a human-readable summary.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let status = if self.success() { "PASS" } else { "FAIL" };
        let mut s = format!("\n{}\n", "═".repeat(60));
        s.push_str(&format!(
            "  E2E Run: {}  |  {} suites  |  {}ms\n",
            status, self.total, self.duration_ms
        ));
        s.push_str(&format!(
            "  Passed: {}  |  Failed: {}  |  Skipped: {}\n",
            self.passed, self.failed, self.skipped
        ));
        s.push_str(&format!("{}\n", "═".repeat(60)));

        // List failures
        if self.failed > 0 {
            s.push_str("\nFailed suites:\n");
            for result in &self.results {
                if !result.passed {
                    s.push_str(&format!(
                        "  - {} (exit {})\n",
                        result.name, result.exit_code
                    ));
                }
            }
        }

        s
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration_classification() {
        assert_eq!(
            SuiteRegistry::classify_duration("cli", &[]),
            DurationClass::Fast
        );
        assert_eq!(
            SuiteRegistry::classify_duration("concurrent_agents", &[]),
            DurationClass::Slow
        );
        assert_eq!(
            SuiteRegistry::classify_duration("http", &[]),
            DurationClass::Normal
        );
        assert_eq!(
            SuiteRegistry::classify_duration("foo", &["slow".to_string()]),
            DurationClass::Slow
        );
    }

    #[test]
    fn test_pattern_matching() {
        assert!(SuiteRegistry::matches_pattern("guard", "guard"));
        assert!(SuiteRegistry::matches_pattern("test_guard", "guard"));
        assert!(SuiteRegistry::matches_pattern("guard_foo", "guard*"));
        assert!(SuiteRegistry::matches_pattern("foo_guard", "*guard"));
        assert!(!SuiteRegistry::matches_pattern("http", "guard"));
    }

    #[test]
    fn test_parse_assertions() {
        let output = "Pass: 27  Fail: 1  Skip: 2";
        let (p, f, s) = Runner::parse_assertions(output);
        assert_eq!(p, 27);
        assert_eq!(f, 1);
        assert_eq!(s, 2);
    }

    #[test]
    fn test_run_report_success() {
        let report = RunReport {
            total: 3,
            passed: 3,
            failed: 0,
            skipped: 0,
            duration_ms: 1000,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
            results: vec![],
        };
        assert!(report.success());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn test_run_report_failure() {
        let report = RunReport {
            total: 3,
            passed: 2,
            failed: 1,
            skipped: 0,
            duration_ms: 1000,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
            results: vec![],
        };
        assert!(!report.success());
        assert_eq!(report.exit_code(), 1);
    }
}
