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
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with("test_")
                && name.ends_with(".sh")
            {
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
                let included = include.is_none_or(|patterns| {
                    patterns
                        .iter()
                        .any(|p| Self::matches_pattern(&suite.name, p))
                });

                // If exclude patterns specified, suite must not match any
                let excluded = exclude.is_some_and(|patterns| {
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
    /// Number of retries after an initial failure.
    pub retries: u32,
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
            retries: 0,
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

#[derive(Debug)]
struct SuiteExecution {
    output: std::process::Output,
    timed_out: bool,
}

impl Runner {
    const NATIVE_HTTP_SUITE: &'static str = "http";
    const NATIVE_DUAL_MODE_SUITE: &'static str = "dual_mode";
    const NATIVE_MODE_MATRIX_SUITE: &'static str = "mode_matrix";
    const NATIVE_SECURITY_PRIVACY_SUITE: &'static str = "security_privacy";
    const NATIVE_TUI_A11Y_SUITE: &'static str = "tui_a11y";

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
        if Self::is_native_suite(&suite.name) {
            return if suite.name == Self::NATIVE_HTTP_SUITE {
                self.run_native_http_suite(suite)
            } else if suite.name == Self::NATIVE_MODE_MATRIX_SUITE {
                self.run_native_mode_matrix_suite(suite)
            } else if suite.name == Self::NATIVE_SECURITY_PRIVACY_SUITE {
                self.run_native_security_privacy_suite(suite)
            } else if suite.name == Self::NATIVE_TUI_A11Y_SUITE {
                self.run_native_tui_a11y_suite(suite)
            } else {
                self.run_native_dual_mode_suite(suite)
            };
        }

        let started_at = Utc::now();
        let start_instant = Instant::now();
        let max_attempts = self.config.retries.saturating_add(1);

        let mut attempts_used = 0u32;
        let mut last_stdout = String::new();
        let mut last_stderr = String::new();
        let mut last_exit_code = -1;
        let mut last_passed = false;
        let mut execution_error = None;

        for attempt in 1..=max_attempts {
            attempts_used = attempt;
            match self.run_suite_once(suite) {
                Ok(execution) => {
                    let stdout = Self::truncate_output(
                        &execution.output.stdout,
                        self.config.max_output_bytes,
                    );
                    let mut stderr = Self::truncate_output(
                        &execution.output.stderr,
                        self.config.max_output_bytes,
                    );

                    let exit_code = if execution.timed_out {
                        124
                    } else {
                        execution.output.status.code().unwrap_or(-1)
                    };
                    let passed = !execution.timed_out && execution.output.status.success();

                    if execution.timed_out {
                        if !stderr.is_empty() {
                            stderr.push('\n');
                        }
                        let timeout_ms = self
                            .config
                            .timeout
                            .map_or(0, |duration| duration.as_millis());
                        stderr.push_str(&format!("Suite timed out after {timeout_ms}ms"));
                    }

                    last_stdout = stdout;
                    last_stderr = stderr;
                    last_exit_code = exit_code;
                    last_passed = passed;

                    if passed {
                        break;
                    }
                }
                Err(error) => {
                    execution_error = Some(format!("Failed to execute suite: {error}"));
                    break;
                }
            }
        }

        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        if let Some(error) = execution_error {
            SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: error,
                assertions_passed: 0,
                assertions_failed: 0,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            }
        } else {
            let (assertions_passed, assertions_failed, assertions_skipped) =
                Self::parse_assertions(&last_stdout);

            if attempts_used > 1 {
                if !last_stderr.is_empty() {
                    last_stderr.push('\n');
                }
                last_stderr.push_str(&format!(
                    "Attempts used: {attempts_used} (max_retries={})",
                    self.config.retries
                ));
            }

            SuiteResult {
                name: suite.name.clone(),
                passed: last_passed,
                exit_code: last_exit_code,
                duration_ms: elapsed.as_millis() as u64,
                stdout: last_stdout,
                stderr: last_stderr,
                assertions_passed,
                assertions_failed,
                assertions_skipped,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            }
        }
    }

    fn run_suite_once(&self, suite: &Suite) -> std::io::Result<SuiteExecution> {
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

        let mut child = cmd.spawn()?;
        let mut timed_out = false;

        if let Some(timeout) = self.config.timeout {
            let timeout_start = Instant::now();
            loop {
                if child.try_wait()?.is_some() {
                    break;
                }

                if timeout_start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }

                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let output = child.wait_with_output()?;
        Ok(SuiteExecution { output, timed_out })
    }

    fn is_native_suite(name: &str) -> bool {
        name == Self::NATIVE_HTTP_SUITE
            || name == Self::NATIVE_DUAL_MODE_SUITE
            || name == Self::NATIVE_MODE_MATRIX_SUITE
            || name == Self::NATIVE_SECURITY_PRIVACY_SUITE
            || name == Self::NATIVE_TUI_A11Y_SUITE
    }

    fn run_native_http_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        let mut cmd = Command::new("cargo");
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "http_transport_harness",
            "--",
            "--nocapture",
        ]);
        cmd.current_dir(&self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        cmd.env("AM_E2E_HTTP_REQUIRE_PASS", "1");
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_HTTP_ARTIFACT_DIR", artifact_root);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output();
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match output {
            Ok(output) => {
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let stderr = Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let passed = output.status.success();
                SuiteResult {
                    name: suite.name.clone(),
                    passed,
                    exit_code: output.status.code().unwrap_or(-1),
                    duration_ms: elapsed.as_millis() as u64,
                    stdout,
                    stderr,
                    assertions_passed: if passed { 1 } else { 0 },
                    assertions_failed: if passed { 0 } else { 1 },
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                }
            }
            Err(error) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute native http suite: {error}"),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    fn run_native_mode_matrix_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        let mut cmd = Command::new("cargo");
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "mode_matrix_harness",
            "--",
            "--nocapture",
        ]);
        cmd.current_dir(&self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_MODE_MATRIX_ARTIFACT_DIR", artifact_root);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output();
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match output {
            Ok(output) => {
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let stderr = Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let passed = output.status.success();
                SuiteResult {
                    name: suite.name.clone(),
                    passed,
                    exit_code: output.status.code().unwrap_or(-1),
                    duration_ms: elapsed.as_millis() as u64,
                    stdout,
                    stderr,
                    assertions_passed: if passed { 1 } else { 0 },
                    assertions_failed: if passed { 0 } else { 1 },
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                }
            }
            Err(error) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute native mode-matrix suite: {error}"),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    fn run_native_security_privacy_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        let mut cmd = Command::new("cargo");
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "security_privacy_harness",
            "--",
            "--nocapture",
        ]);
        cmd.current_dir(&self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_SECURITY_PRIVACY_ARTIFACT_DIR", artifact_root);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output();
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match output {
            Ok(output) => {
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let stderr = Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let passed = output.status.success();
                SuiteResult {
                    name: suite.name.clone(),
                    passed,
                    exit_code: output.status.code().unwrap_or(-1),
                    duration_ms: elapsed.as_millis() as u64,
                    stdout,
                    stderr,
                    assertions_passed: if passed { 1 } else { 0 },
                    assertions_failed: if passed { 0 } else { 1 },
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                }
            }
            Err(error) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute native security/privacy suite: {error}"),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    fn run_native_tui_a11y_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        let mut cmd = Command::new("cargo");
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "tui_accessibility_harness",
            "--",
            "--nocapture",
        ]);
        cmd.current_dir(&self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_TUI_A11Y_ARTIFACT_DIR", artifact_root);
        }
        // CI-quality gate: skipping keyboard/adapter cases is not acceptable.
        cmd.env("AM_E2E_TUI_A11Y_REQUIRE_NO_SKIP", "1");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output();
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match output {
            Ok(output) => {
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let stderr = Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let passed = output.status.success();
                SuiteResult {
                    name: suite.name.clone(),
                    passed,
                    exit_code: output.status.code().unwrap_or(-1),
                    duration_ms: elapsed.as_millis() as u64,
                    stdout,
                    stderr,
                    assertions_passed: if passed { 1 } else { 0 },
                    assertions_failed: if passed { 0 } else { 1 },
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                }
            }
            Err(error) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute native tui_a11y suite: {error}"),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    fn run_native_dual_mode_suite(&self, suite: &Suite) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();

        let mut assertions_passed = 0u32;
        let mut assertions_failed = 0u32;
        let assertions_skipped = 0u32;
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        let artifact_root = self.config.artifact_dir.as_ref().map(|base| {
            base.join(&suite.name)
                .join(Utc::now().format("%Y%m%d_%H%M%S").to_string())
        });

        if let Some(root) = &artifact_root {
            if let Err(error) = fs::create_dir_all(root.join("steps")) {
                stderr_lines.push(format!(
                    "Failed to create dual-mode artifact steps directory {}: {error}",
                    root.display()
                ));
            }
            if let Err(error) = fs::create_dir_all(root.join("failures")) {
                stderr_lines.push(format!(
                    "Failed to create dual-mode artifact failures directory {}: {error}",
                    root.display()
                ));
            }
        }

        let (cli_bin, mcp_bin) = match self.ensure_dual_mode_binaries() {
            Ok(paths) => paths,
            Err(error) => {
                let elapsed = start_instant.elapsed();
                let ended_at = Utc::now();
                return SuiteResult {
                    name: suite.name.clone(),
                    passed: false,
                    exit_code: 1,
                    duration_ms: elapsed.as_millis() as u64,
                    stdout: String::new(),
                    stderr: error,
                    assertions_passed: 0,
                    assertions_failed: 1,
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                };
            }
        };

        let temp_dir = match tempfile::TempDir::new() {
            Ok(temp) => temp,
            Err(error) => {
                let elapsed = start_instant.elapsed();
                let ended_at = Utc::now();
                return SuiteResult {
                    name: suite.name.clone(),
                    passed: false,
                    exit_code: 1,
                    duration_ms: elapsed.as_millis() as u64,
                    stdout: String::new(),
                    stderr: format!("Failed to create temporary dual-mode workspace: {error}"),
                    assertions_passed: 0,
                    assertions_failed: 1,
                    assertions_skipped: 0,
                    started_at: started_at.to_rfc3339(),
                    ended_at: ended_at.to_rfc3339(),
                };
            }
        };
        let storage_root = temp_dir.path().join("storage");
        if let Err(error) = fs::create_dir_all(&storage_root) {
            let elapsed = start_instant.elapsed();
            let ended_at = Utc::now();
            return SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: 1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to create dual-mode storage directory: {error}"),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            };
        }

        let mut env_map = HashMap::new();
        env_map.insert(
            "DATABASE_URL".to_string(),
            format!("sqlite:///{}/test.sqlite3", temp_dir.path().display()),
        );
        env_map.insert(
            "STORAGE_ROOT".to_string(),
            storage_root.display().to_string(),
        );
        env_map.insert("AGENT_NAME".to_string(), "DualModeTest".to_string());
        env_map.insert("HTTP_HOST".to_string(), "127.0.0.1".to_string());
        env_map.insert("HTTP_PORT".to_string(), "1".to_string());
        env_map.insert("HTTP_PATH".to_string(), "/mcp/".to_string());
        if self.config.keep_tmp {
            env_map.insert("AM_E2E_KEEP_TMP".to_string(), "1".to_string());
        }
        for (key, value) in &self.config.env {
            env_map.insert(key.clone(), value.clone());
        }

        let mut step_index = 0usize;
        let mut step_failures = 0usize;

        let mut record_check = |label: &str,
                                binary_label: &str,
                                command: &str,
                                mode: &str,
                                expected_decision: &str,
                                exit_code: i32,
                                stdout_excerpt: &str,
                                stderr_excerpt: &str,
                                passed: bool| {
            if passed {
                assertions_passed += 1;
                stdout_lines.push(format!("PASS {label}"));
            } else {
                assertions_failed += 1;
                step_failures += 1;
                stdout_lines.push(format!("FAIL {label}"));
                stderr_lines.push(format!(
                    "{label} failed (exit={exit_code}): {}",
                    if stderr_excerpt.is_empty() {
                        stdout_excerpt
                    } else {
                        stderr_excerpt
                    }
                ));
            }

            Self::write_dual_mode_step_artifact(
                &artifact_root,
                &mut step_index,
                binary_label,
                command,
                mode,
                expected_decision,
                exit_code,
                stdout_excerpt,
                stderr_excerpt,
                passed,
            );
        };

        const CLI_ALLOW: &[&str] = &[
            "serve-http --help",
            "serve-stdio --help",
            "share --help",
            "archive --help",
            "guard --help",
            "acks --help",
            "list-acks --help",
            "migrate --help",
            "list-projects --help",
            "clear-and-reset-everything --help",
            "config --help",
            "amctl --help",
            "projects --help",
            "mail --help",
            "products --help",
            "docs --help",
            "doctor --help",
            "agents --help",
            "tooling --help",
            "macros --help",
            "contacts --help",
            "file_reservations --help",
        ];
        for entry in CLI_ALLOW {
            let args: Vec<&str> = entry.split_whitespace().collect();
            match self.run_dual_mode_command(&cli_bin, &args, &env_map) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let passed = exit_code == 0;
                    record_check(
                        &format!("CLI allows {}", args[0]),
                        "am",
                        entry,
                        "cli",
                        "allow",
                        exit_code,
                        &stdout_excerpt,
                        &stderr_excerpt,
                        passed,
                    );
                }
                Err(error) => record_check(
                    &format!("CLI allows {}", args[0]),
                    "am",
                    entry,
                    "cli",
                    "allow",
                    -1,
                    "",
                    &error.to_string(),
                    false,
                ),
            }
        }

        const MCP_DENY: &[&str] = &[
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
        for command in MCP_DENY {
            let args = [*command];
            match self.run_dual_mode_command(&mcp_bin, &args, &env_map) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let passed = exit_code == 2;
                    record_check(
                        &format!("MCP denies {command}"),
                        "mcp-agent-mail",
                        command,
                        "mcp",
                        "deny",
                        exit_code,
                        &stdout_excerpt,
                        &stderr_excerpt,
                        passed,
                    );
                }
                Err(error) => record_check(
                    &format!("MCP denies {command}"),
                    "mcp-agent-mail",
                    command,
                    "mcp",
                    "deny",
                    -1,
                    "",
                    &error.to_string(),
                    false,
                ),
            }
        }

        const MCP_ALLOW: &[&str] = &["serve --help", "config", "--help", "--version"];
        for entry in MCP_ALLOW {
            let args: Vec<&str> = entry.split_whitespace().collect();
            match self.run_dual_mode_command(&mcp_bin, &args, &env_map) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let passed = exit_code == 0;
                    record_check(
                        &format!("MCP allows {entry}"),
                        "mcp-agent-mail",
                        entry,
                        "mcp",
                        "allow",
                        exit_code,
                        &stdout_excerpt,
                        &stderr_excerpt,
                        passed,
                    );
                }
                Err(error) => record_check(
                    &format!("MCP allows {entry}"),
                    "mcp-agent-mail",
                    entry,
                    "mcp",
                    "allow",
                    -1,
                    "",
                    &error.to_string(),
                    false,
                ),
            }
        }

        const DENIAL_TEST_CMDS: &[&str] = &["share", "guard", "doctor", "archive", "migrate"];
        for command in DENIAL_TEST_CMDS {
            let args = [*command];
            match self.run_dual_mode_command(&mcp_bin, &args, &env_map) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                    let checks = [
                        (
                            "mentions command",
                            stderr.contains(&format!("\"{command}\"")),
                            format!("stderr missing \"{command}\""),
                        ),
                        (
                            "has remediation",
                            stderr.contains(&format!("am {command}")),
                            format!("stderr missing remediation for {command}"),
                        ),
                        (
                            "lists accepted commands",
                            stderr.contains("serve, config"),
                            "stderr missing accepted command list".to_string(),
                        ),
                        (
                            "no panic",
                            !stderr.contains("panicked"),
                            "stderr unexpectedly contains panic".to_string(),
                        ),
                        (
                            "no backtrace",
                            !stderr.contains("stack backtrace"),
                            "stderr unexpectedly contains backtrace".to_string(),
                        ),
                        (
                            "stdout empty",
                            stdout.trim().is_empty(),
                            "stdout must be empty for denial cases".to_string(),
                        ),
                        (
                            "exit code is 2",
                            exit_code == 2,
                            format!("expected exit code 2, got {exit_code}"),
                        ),
                    ];
                    for (check_name, passed, detail) in checks {
                        let stderr_for_record = if passed {
                            stderr_excerpt.as_str()
                        } else {
                            detail.as_str()
                        };
                        record_check(
                            &format!("Denial contract [{command}] {check_name}"),
                            "mcp-agent-mail",
                            command,
                            "mcp",
                            "deny_contract",
                            exit_code,
                            &stdout_excerpt,
                            stderr_for_record,
                            passed,
                        );
                    }
                }
                Err(error) => record_check(
                    &format!("Denial contract [{command}] execution"),
                    "mcp-agent-mail",
                    command,
                    "mcp",
                    "deny_contract",
                    -1,
                    "",
                    &error.to_string(),
                    false,
                ),
            }
        }

        const ENV_OVERRIDES: &[(&str, &str)] = &[
            ("INTERFACE_MODE", "agent"),
            ("INTERFACE_MODE", "cli"),
            ("MCP_MODE", "agent"),
        ];
        for (env_key, env_value) in ENV_OVERRIDES {
            let mut override_env = env_map.clone();
            override_env.insert((*env_key).to_string(), (*env_value).to_string());
            let command_text = format!("share ({env_key}={env_value})");
            let args = ["share"];
            match self.run_dual_mode_command(&mcp_bin, &args, &override_env) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let passed = exit_code == 2;
                    record_check(
                        &format!("Env override cannot bypass denial: {env_key}={env_value}"),
                        "mcp-agent-mail",
                        &command_text,
                        "mcp-env-override",
                        "deny",
                        exit_code,
                        &stdout_excerpt,
                        &stderr_excerpt,
                        passed,
                    );
                }
                Err(error) => record_check(
                    &format!("Env override cannot bypass denial: {env_key}={env_value}"),
                    "mcp-agent-mail",
                    &command_text,
                    "mcp-env-override",
                    "deny",
                    -1,
                    "",
                    &error.to_string(),
                    false,
                ),
            }
        }

        let cli_config = self.run_dual_mode_command(&cli_bin, &["config", "--help"], &env_map);
        let mcp_config = self.run_dual_mode_command(&mcp_bin, &["config"], &env_map);
        match (cli_config, mcp_config) {
            (Ok(cli_out), Ok(mcp_out)) => {
                let cli_exit = cli_out.status.code().unwrap_or(-1);
                let mcp_exit = mcp_out.status.code().unwrap_or(-1);
                let passed = cli_exit == 0 && mcp_exit == 0;
                let summary = format!("cli={cli_exit} mcp={mcp_exit}");
                record_check(
                    "config accepted by both binaries",
                    "am+mcp-agent-mail",
                    "config parity",
                    "cross-mode",
                    "allow",
                    if passed { 0 } else { 1 },
                    &summary,
                    "",
                    passed,
                );
            }
            (Err(error), _) => record_check(
                "config accepted by both binaries",
                "am+mcp-agent-mail",
                "config parity",
                "cross-mode",
                "allow",
                -1,
                "",
                &format!("CLI config command failed: {error}"),
                false,
            ),
            (_, Err(error)) => record_check(
                "config accepted by both binaries",
                "am+mcp-agent-mail",
                "config parity",
                "cross-mode",
                "allow",
                -1,
                "",
                &format!("MCP config command failed: {error}"),
                false,
            ),
        }

        let cli_functional_checks: [(&str, &[&str], Option<&str>); 6] = [
            ("CLI migrate exits 0", &["migrate"], None),
            (
                "CLI doctor check exits 0",
                &["doctor", "check", "--json"],
                Some("healthy"),
            ),
            (
                "CLI list-projects exits 0",
                &["list-projects", "--json"],
                None,
            ),
            (
                "CLI tooling directory exits 0",
                &["tooling", "directory", "--json"],
                Some("clusters"),
            ),
            (
                "CLI tooling schemas exits 0",
                &["tooling", "schemas", "--json"],
                None,
            ),
            (
                "CLI agents list --help exits 0",
                &["agents", "list", "--help"],
                None,
            ),
        ];
        for (label, args, required_text) in cli_functional_checks {
            match self.run_dual_mode_command(&cli_bin, args, &env_map) {
                Ok(output) => {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stdout_text = String::from_utf8_lossy(&output.stdout).into_owned();
                    let stdout_excerpt = Self::output_excerpt(&output.stdout, 500);
                    let stderr_excerpt = Self::output_excerpt(&output.stderr, 500);
                    let required_ok =
                        required_text.is_none_or(|needle| stdout_text.contains(needle));
                    let passed = exit_code == 0 && required_ok;
                    let mut command_text = String::new();
                    command_text.push_str("am ");
                    command_text.push_str(&args.join(" "));
                    record_check(
                        label,
                        "am",
                        &command_text,
                        "cli-functional",
                        "allow",
                        exit_code,
                        &stdout_excerpt,
                        &stderr_excerpt,
                        passed,
                    );
                }
                Err(error) => {
                    let mut command_text = String::new();
                    command_text.push_str("am ");
                    command_text.push_str(&args.join(" "));
                    record_check(
                        label,
                        "am",
                        &command_text,
                        "cli-functional",
                        "allow",
                        -1,
                        "",
                        &error.to_string(),
                        false,
                    );
                }
            }
        }

        if let Some(root) = &artifact_root {
            if let Err(error) = Self::write_dual_mode_summary_artifact(
                root,
                step_index,
                step_failures,
                assertions_passed,
                assertions_failed,
                assertions_skipped,
            ) {
                stderr_lines.push(format!(
                    "Failed to write dual-mode summary artifact under {}: {error}",
                    root.display()
                ));
            } else {
                stdout_lines.push(format!("ARTIFACT_DIR={}", root.display()));
            }
        }

        let passed = assertions_failed == 0;
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        SuiteResult {
            name: suite.name.clone(),
            passed,
            exit_code: if passed { 0 } else { 1 },
            duration_ms: elapsed.as_millis() as u64,
            stdout: stdout_lines.join("\n"),
            stderr: stderr_lines.join("\n"),
            assertions_passed,
            assertions_failed,
            assertions_skipped,
            started_at: started_at.to_rfc3339(),
            ended_at: ended_at.to_rfc3339(),
        }
    }

    fn ensure_dual_mode_binaries(&self) -> Result<(PathBuf, PathBuf), String> {
        let target_dir = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| self.config.project_root.join("target"));
        let cli_bin = target_dir.join("debug/am");
        let mcp_bin = target_dir.join("debug/mcp-agent-mail");

        let build_package = |package: &str| -> Result<(), String> {
            let status = Command::new("cargo")
                .args(["build", "-p", package])
                .current_dir(&self.config.project_root)
                .status()
                .map_err(|error| format!("Failed to run cargo build for {package}: {error}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "cargo build -p {package} failed with exit code {:?}",
                    status.code()
                ))
            }
        };

        if self.config.force_build || !cli_bin.exists() {
            build_package("mcp-agent-mail-cli")?;
        }
        if self.config.force_build || !mcp_bin.exists() {
            build_package("mcp-agent-mail")?;
        }

        if !cli_bin.exists() {
            return Err(format!(
                "CLI binary not found at {} after build",
                cli_bin.display()
            ));
        }
        if !mcp_bin.exists() {
            return Err(format!(
                "MCP binary not found at {} after build",
                mcp_bin.display()
            ));
        }

        Ok((cli_bin, mcp_bin))
    }

    fn run_dual_mode_command(
        &self,
        binary: &Path,
        args: &[&str],
        env_map: &HashMap<String, String>,
    ) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new(binary);
        cmd.args(args);
        cmd.current_dir(&self.config.project_root);
        for (key, value) in env_map {
            cmd.env(key, value);
        }
        cmd.output()
    }

    fn output_excerpt(bytes: &[u8], max_chars: usize) -> String {
        let text = String::from_utf8_lossy(bytes);
        if text.chars().count() <= max_chars {
            text.into_owned()
        } else {
            let mut truncated = text.chars().take(max_chars).collect::<String>();
            truncated.push_str("...");
            truncated
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_dual_mode_step_artifact(
        artifact_root: &Option<PathBuf>,
        step_index: &mut usize,
        binary: &str,
        command: &str,
        mode: &str,
        expected_decision: &str,
        exit_code: i32,
        stdout_excerpt: &str,
        stderr_excerpt: &str,
        passed: bool,
    ) {
        let Some(root) = artifact_root else {
            return;
        };

        *step_index += 1;
        let step_id = format!("{:03}", *step_index);
        let step_path = root.join("steps").join(format!("step_{step_id}.json"));
        let payload = serde_json::json!({
            "step_id": step_id.clone(),
            "timestamp": Utc::now().to_rfc3339(),
            "binary": binary,
            "command": command,
            "mode": mode,
            "mode_provenance": "native-e2e-runner",
            "expected_decision": expected_decision,
            "actual_exit_code": exit_code,
            "stdout_excerpt": stdout_excerpt,
            "stderr_excerpt": stderr_excerpt,
            "passed": passed,
        });
        if let Ok(file) = fs::File::create(step_path) {
            let _ = serde_json::to_writer_pretty(file, &payload);
        }

        if !passed {
            let fail_path = root.join("failures").join(format!("fail_{step_id}.json"));
            let failure = serde_json::json!({
                "step_id": step_id,
                "binary": binary,
                "command": command,
                "mode": mode,
                "expected_decision": expected_decision,
                "actual_exit_code": exit_code,
                "stdout": stdout_excerpt,
                "stderr": stderr_excerpt,
                "reproduction": format!("{binary} {command}"),
            });
            if let Ok(file) = fs::File::create(fail_path) {
                let _ = serde_json::to_writer_pretty(file, &failure);
            }
        }
    }

    fn write_dual_mode_summary_artifact(
        artifact_root: &Path,
        total_steps: usize,
        step_failures: usize,
        assertions_passed: u32,
        assertions_failed: u32,
        assertions_skipped: u32,
    ) -> std::io::Result<()> {
        let summary = serde_json::json!({
            "suite": "dual_mode",
            "runner": "native",
            "total_steps": total_steps,
            "step_failures": step_failures,
            "e2e_pass": assertions_passed,
            "e2e_fail": assertions_failed,
            "e2e_skip": assertions_skipped,
            "generated_at": Utc::now().to_rfc3339(),
        });
        let file = fs::File::create(artifact_root.join("run_summary.json"))?;
        serde_json::to_writer_pretty(file, &summary)?;
        Ok(())
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
                        if let Some(num) = words.get(i + 1)
                            && let Ok(n) = num.parse::<u32>()
                        {
                            passed = n;
                        }
                    } else if word_lower == "fail:" {
                        if let Some(num) = words.get(i + 1)
                            && let Ok(n) = num.parse::<u32>()
                        {
                            failed = n;
                        }
                    } else if word_lower == "skip:"
                        && let Some(num) = words.get(i + 1)
                        && let Ok(n) = num.parse::<u32>()
                    {
                        skipped = n;
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
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_suite_script(project_root: &Path, suite_name: &str, body: &str) -> PathBuf {
        let e2e_dir = project_root.join("tests/e2e");
        fs::create_dir_all(&e2e_dir).expect("create tests/e2e");
        let script_path = e2e_dir.join(format!("test_{suite_name}.sh"));
        fs::write(&script_path, body).expect("write suite script");
        script_path
    }

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

    #[test]
    fn test_suite_registry_discovery_extracts_metadata_and_sorts_names() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "alpha",
            r#"#!/usr/bin/env bash
# Alpha suite description
# @tags: slow, flaky
echo "Pass: 1  Fail: 0  Skip: 0"
"#,
        );
        write_suite_script(
            temp.path(),
            "beta",
            r#"#!/usr/bin/env bash
# Beta suite description
echo "Pass: 2  Fail: 0  Skip: 0"
"#,
        );

        let registry = SuiteRegistry::new(temp.path()).expect("registry creation");
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.suite_names(), vec!["alpha", "beta"]);

        let alpha = registry.get("alpha").expect("alpha suite");
        assert_eq!(
            alpha.description.as_deref(),
            Some("Alpha suite description")
        );
        assert_eq!(alpha.tags, vec!["slow", "flaky"]);
        assert_eq!(alpha.duration_class, DurationClass::Slow);

        let beta = registry.get("beta").expect("beta suite");
        assert_eq!(beta.description.as_deref(), Some("Beta suite description"));
        assert!(beta.tags.is_empty());
    }

    #[test]
    fn test_runner_run_filtered_include_and_exclude_patterns() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "pass",
            r#"#!/usr/bin/env bash
echo "Total: 1  Pass: 3  Fail: 0  Skip: 1"
exit 0
"#,
        );
        write_suite_script(
            temp.path(),
            "fail",
            r#"#!/usr/bin/env bash
echo "Total: 1  Pass: 1  Fail: 1  Skip: 0"
exit 1
"#,
        );

        let config = RunConfig {
            project_root: temp.path().to_path_buf(),
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let runner = Runner::new(temp.path(), config).expect("runner");

        let include = vec!["f*".to_string()];
        let report_include = runner.run_filtered(Some(&include), None);
        assert_eq!(report_include.total, 1);
        assert_eq!(report_include.failed, 1);
        assert_eq!(report_include.results[0].name, "fail");

        let exclude = vec!["fail".to_string()];
        let report_exclude = runner.run_filtered(None, Some(&exclude));
        assert_eq!(report_exclude.total, 1);
        assert_eq!(report_exclude.passed, 1);
        assert_eq!(report_exclude.results[0].name, "pass");
    }

    #[test]
    fn test_runner_truncates_output_and_parses_ansi_assertion_summary() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "ansi",
            r#"#!/usr/bin/env bash
printf "\033[32mPass: 4\033[0m  \033[31mFail: 1\033[0m  \033[33mSkip: 2\033[0m\n"
printf "012345678901234567890123456789\n"
exit 1
"#,
        );

        let config = RunConfig {
            project_root: temp.path().to_path_buf(),
            max_output_bytes: 72,
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let runner = Runner::new(temp.path(), config).expect("runner");
        let report = runner.run(&["ansi".to_string()]);

        assert_eq!(report.total, 1);
        let result = &report.results[0];
        assert!(result.stdout.contains("output truncated"));
        assert_eq!(result.assertions_passed, 4);
        assert_eq!(result.assertions_failed, 1);
        assert_eq!(result.assertions_skipped, 2);
    }

    #[test]
    fn test_runner_timeout_marks_suite_failed_with_timeout_code() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "timeout",
            r#"#!/usr/bin/env bash
sleep 1
echo "Pass: 1  Fail: 0  Skip: 0"
exit 0
"#,
        );

        let config = RunConfig {
            project_root: temp.path().to_path_buf(),
            timeout: Some(Duration::from_millis(100)),
            ..Default::default()
        };
        let runner = Runner::new(temp.path(), config).expect("runner");
        let report = runner.run(&["timeout".to_string()]);
        let result = &report.results[0];

        assert!(!result.passed);
        assert_eq!(result.exit_code, 124);
        assert!(result.stderr.contains("timed out"));
    }

    #[test]
    fn test_runner_retries_failed_suite_until_success() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "flaky",
            r#"#!/usr/bin/env bash
MARKER="${E2E_PROJECT_ROOT}/retry_marker"
if [ -f "${MARKER}" ]; then
  echo "Pass: 2  Fail: 0  Skip: 0"
  exit 0
fi
touch "${MARKER}"
echo "Pass: 0  Fail: 1  Skip: 0"
exit 1
"#,
        );

        let config = RunConfig {
            project_root: temp.path().to_path_buf(),
            retries: 1,
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let runner = Runner::new(temp.path(), config).expect("runner");
        let report = runner.run(&["flaky".to_string()]);
        let result = &report.results[0];

        assert!(result.passed);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.assertions_passed, 2);
        assert!(result.stderr.contains("Attempts used: 2"));
    }

    #[test]
    fn test_run_report_summary_lists_failed_suite_names() {
        let report = RunReport {
            total: 2,
            passed: 1,
            failed: 1,
            skipped: 0,
            duration_ms: 250,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
            results: vec![
                SuiteResult {
                    name: "alpha".to_string(),
                    passed: true,
                    exit_code: 0,
                    duration_ms: 100,
                    stdout: String::new(),
                    stderr: String::new(),
                    assertions_passed: 1,
                    assertions_failed: 0,
                    assertions_skipped: 0,
                    started_at: "2026-02-12T00:00:00Z".to_string(),
                    ended_at: "2026-02-12T00:00:00Z".to_string(),
                },
                SuiteResult {
                    name: "beta".to_string(),
                    passed: false,
                    exit_code: 7,
                    duration_ms: 150,
                    stdout: String::new(),
                    stderr: "boom".to_string(),
                    assertions_passed: 0,
                    assertions_failed: 1,
                    assertions_skipped: 0,
                    started_at: "2026-02-12T00:00:00Z".to_string(),
                    ended_at: "2026-02-12T00:00:01Z".to_string(),
                },
            ],
        };

        let summary = report.format_summary();
        assert!(summary.contains("E2E Run: FAIL"));
        assert!(summary.contains("Failed suites:"));
        assert!(summary.contains("beta (exit 7)"));
    }

    #[test]
    fn test_native_suite_detection_matches_enabled_native_suites() {
        assert!(Runner::is_native_suite("http"));
        assert!(Runner::is_native_suite("dual_mode"));
        assert!(Runner::is_native_suite("mode_matrix"));
        assert!(Runner::is_native_suite("security_privacy"));
        assert!(Runner::is_native_suite("tui_a11y"));
        assert!(!Runner::is_native_suite("guard"));
        assert!(!Runner::is_native_suite("dual_mode_extra"));
    }

    #[test]
    fn test_write_dual_mode_step_artifact_creates_step_and_failure_entries() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("dual_mode").join("20260213_000000");
        fs::create_dir_all(root.join("steps")).expect("steps dir");
        fs::create_dir_all(root.join("failures")).expect("failures dir");

        let artifact_root = Some(root.clone());
        let mut step_index = 0usize;
        Runner::write_dual_mode_step_artifact(
            &artifact_root,
            &mut step_index,
            "am",
            "share --help",
            "cli",
            "allow",
            1,
            "",
            "boom",
            false,
        );

        let step_path = root.join("steps/step_001.json");
        let fail_path = root.join("failures/fail_001.json");
        assert!(step_path.exists());
        assert!(fail_path.exists());

        let step_value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(step_path).expect("read step"))
                .expect("parse step");
        assert_eq!(step_value["binary"], "am");
        assert_eq!(step_value["expected_decision"], "allow");
        assert_eq!(step_value["passed"], false);
    }

    #[test]
    fn test_write_dual_mode_summary_artifact_writes_expected_counts() {
        let temp = TempDir::new().expect("tempdir");
        Runner::write_dual_mode_summary_artifact(temp.path(), 12, 2, 30, 2, 0)
            .expect("write summary");

        let summary_path = temp.path().join("run_summary.json");
        assert!(summary_path.exists());
        let summary: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(summary_path).expect("read summary"))
                .expect("parse summary");
        assert_eq!(summary["suite"], "dual_mode");
        assert_eq!(summary["runner"], "native");
        assert_eq!(summary["total_steps"], 12);
        assert_eq!(summary["step_failures"], 2);
        assert_eq!(summary["e2e_pass"], 30);
        assert_eq!(summary["e2e_fail"], 2);
    }
}
