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

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
#[derive(Debug, Clone)]
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
                    .expect("already checked prefix")
                    .strip_suffix(".sh")
                    .expect("already checked suffix")
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

    /// Filters suites by include/exclude patterns and `@tags:` membership.
    ///
    /// Tags are matched exactly (no globbing): a suite qualifies when it
    /// carries at least one of the requested tags. All three filters compose
    /// as AND: (include if any) AND (tags if any) AND NOT (exclude).
    pub fn filter(
        &self,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
        tags: Option<&[String]>,
    ) -> Vec<&Suite> {
        self.suites()
            .into_iter()
            .filter(|suite| {
                // If include patterns specified, suite must match at least one
                let included = include.is_none_or(|patterns| {
                    patterns
                        .iter()
                        .any(|p| Self::matches_pattern(&suite.name, p))
                });

                // If tags specified, suite must carry at least one of them
                let tagged = tags.is_none_or(|wanted| {
                    wanted
                        .iter()
                        .any(|t| suite.tags.iter().any(|have| have == t))
                });

                // If exclude patterns specified, suite must not match any
                let excluded = exclude.is_some_and(|patterns| {
                    patterns
                        .iter()
                        .any(|p| Self::matches_pattern(&suite.name, p))
                });

                included && tagged && !excluded
            })
            .collect()
    }

    /// Simple glob-like pattern matching.
    fn matches_pattern(name: &str, pattern: &str) -> bool {
        if !pattern.contains('*') {
            return name == pattern || name.contains(pattern);
        }

        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.is_empty() {
            return true;
        }

        let mut current_name = name;
        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                if !current_name.starts_with(part) {
                    return false;
                }
                current_name = &current_name[part.len()..];
            } else if i == parts.len() - 1 {
                if !current_name.ends_with(part) {
                    return false;
                }
            } else {
                if part.is_empty() {
                    continue;
                }
                if let Some(pos) = current_name.find(part) {
                    current_name = &current_name[pos + part.len()..];
                } else {
                    return false;
                }
            }
        }
        true
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
    /// Bind release evidence to this invocation and enforce its coverage.
    pub release_scorecard: bool,
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
            release_scorecard: false,
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
    /// Shared deadline for a native suite made up of multiple child commands.
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct SuiteExecution {
    output: std::process::Output,
    timed_out: bool,
    capture_incomplete: bool,
}

impl Runner {
    const NATIVE_HTTP_SUITE: &'static str = "http";
    const NATIVE_HTTP_STREAMABLE_SUITE: &'static str = "http_streamable";
    const NATIVE_MCP_API_PARITY_SUITE: &'static str = "mcp_api_parity";
    const NATIVE_SHARE_SUITE: &'static str = "share";
    const NATIVE_SHARE_VERIFY_LIVE_SUITE: &'static str = "share_verify_live";
    const NATIVE_ARCHIVE_SUITE: &'static str = "archive";
    const NATIVE_DUAL_MODE_SUITE: &'static str = "dual_mode";
    const NATIVE_MODE_MATRIX_SUITE: &'static str = "mode_matrix";
    const NATIVE_SECURITY_PRIVACY_SUITE: &'static str = "security_privacy";
    const NATIVE_TUI_INTERACTION_SUITE: &'static str = "tui_interaction";
    const NATIVE_TUI_INTERACTIONS_SUITE: &'static str = "tui_interactions";
    const NATIVE_TUI_COMPAT_MATRIX_SUITE: &'static str = "tui_compat_matrix";
    const NATIVE_TUI_STARTUP_SUITE: &'static str = "tui_startup";
    const NATIVE_TUI_A11Y_SUITE: &'static str = "tui_a11y";

    /// Creates a new runner.
    pub fn new(project_root: impl AsRef<Path>, config: RunConfig) -> std::io::Result<Self> {
        let registry = SuiteRegistry::new(project_root)?;
        Ok(Self {
            registry,
            config,
            deadline: None,
        })
    }

    /// Returns the suite registry.
    #[must_use]
    pub fn registry(&self) -> &SuiteRegistry {
        &self.registry
    }

    /// Runs the specified suites (or all if empty).
    pub fn run(&self, suite_names: &[String]) -> RunReport {
        let selected = if suite_names.is_empty() {
            self.registry.suite_names()
        } else {
            suite_names.to_vec()
        };
        self.run_selected(&selected)
    }

    /// Executes an already resolved selection. An empty selection must stay
    /// empty: treating it as "all" would undo an explicit filter.
    fn run_selected(&self, suite_names: &[String]) -> RunReport {
        let run_started = Utc::now();
        let start_instant = Instant::now();
        let mut results = Vec::with_capacity(suite_names.len());
        let mut passed = 0;
        let mut failed = 0;
        let mut execution_config = self.config.clone();
        let evidence = if self.config.release_scorecard {
            match ReleaseRunEvidence::prepare(&self.config, suite_names) {
                Ok(evidence) => {
                    execution_config.artifact_dir = Some(evidence.directory.clone());
                    execution_config.env.insert(
                        "AM_E2E_RELEASE_RUN".to_string(),
                        serde_json::to_string(&evidence).expect("release evidence serializes"),
                    );
                    Some(evidence)
                }
                Err(error) => {
                    return RunReport {
                        total: 1,
                        passed: 0,
                        failed: 1,
                        skipped: 0,
                        duration_ms: start_instant.elapsed().as_millis() as u64,
                        started_at: run_started.to_rfc3339(),
                        ended_at: Utc::now().to_rfc3339(),
                        results: vec![SuiteResult {
                            name: "release_evidence_setup".to_string(),
                            passed: false,
                            exit_code: 1,
                            duration_ms: 0,
                            stdout: String::new(),
                            stderr: error.to_string(),
                            assertions_passed: 0,
                            assertions_failed: 0,
                            assertions_skipped: 0,
                            started_at: run_started.to_rfc3339(),
                            ended_at: Utc::now().to_rfc3339(),
                        }],
                        evidence: None,
                    };
                }
            }
        } else {
            None
        };
        let execution_runner = Self {
            registry: self.registry.clone(),
            config: execution_config,
            deadline: None,
        };

        for name in suite_names {
            let result = self.registry.get(name).map_or_else(
                || SuiteResult {
                    name: name.clone(),
                    passed: false,
                    exit_code: 2,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: format!("Suite not found: {name}"),
                    assertions_passed: 0,
                    assertions_failed: 0,
                    assertions_skipped: 0,
                    started_at: run_started.to_rfc3339(),
                    ended_at: Utc::now().to_rfc3339(),
                },
                |suite| execution_runner.run_suite(suite),
            );
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
            total: suite_names.len() as u32,
            passed,
            failed,
            skipped: 0,
            duration_ms: elapsed.as_millis() as u64,
            started_at: run_started.to_rfc3339(),
            ended_at: run_ended.to_rfc3339(),
            results,
            evidence,
        }
    }

    /// Runs suites with include/exclude/tag filtering.
    pub fn run_filtered(
        &self,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
        tags: Option<&[String]>,
    ) -> RunReport {
        let suites = self.registry.filter(include, exclude, tags);
        let suite_names: Vec<String> = suites.iter().map(|s| s.name.clone()).collect();
        self.run_selected(&suite_names)
    }

    /// Runs a single suite.
    fn run_suite(&self, suite: &Suite) -> SuiteResult {
        if Self::is_native_suite(&suite.name) {
            return if suite.name == Self::NATIVE_HTTP_SUITE
                || suite.name == Self::NATIVE_HTTP_STREAMABLE_SUITE
                || suite.name == Self::NATIVE_MCP_API_PARITY_SUITE
            {
                self.run_native_http_suite(suite)
            } else if suite.name == Self::NATIVE_SHARE_SUITE
                || suite.name == Self::NATIVE_SHARE_VERIFY_LIVE_SUITE
                || suite.name == Self::NATIVE_ARCHIVE_SUITE
            {
                self.run_native_share_archive_suite(suite)
            } else if suite.name == Self::NATIVE_MODE_MATRIX_SUITE {
                self.run_native_mode_matrix_suite(suite)
            } else if suite.name == Self::NATIVE_SECURITY_PRIVACY_SUITE {
                self.run_native_security_privacy_suite(suite)
            } else if suite.name == Self::NATIVE_TUI_INTERACTION_SUITE
                || suite.name == Self::NATIVE_TUI_INTERACTIONS_SUITE
                || suite.name == Self::NATIVE_TUI_COMPAT_MATRIX_SUITE
                || suite.name == Self::NATIVE_TUI_STARTUP_SUITE
            {
                self.run_native_tui_transport_suite(suite)
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

                    let (assertions_passed, assertions_failed, _) = Self::parse_assertions(&stdout);
                    let assertions_valid = assertions_passed > 0 && assertions_failed == 0;
                    let exit_code = if execution.timed_out {
                        124
                    } else if execution.capture_incomplete {
                        125
                    } else if execution.output.status.success() && !assertions_valid {
                        1
                    } else {
                        execution.output.status.code().unwrap_or(-1)
                    };
                    let passed = !execution.timed_out
                        && !execution.capture_incomplete
                        && execution.output.status.success()
                        && assertions_valid;

                    if execution.output.status.success() && !assertions_valid {
                        stderr.push_str("\nSuite did not report a nonzero passing assertion count with zero failed assertions");
                    }

                    if execution.capture_incomplete {
                        stderr.push_str("\nSuite output exceeded its capture limit or a child retained its output pipes after the suite exited");
                    }

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

    /// Suites must not inherit the operator's interface mode: the documented
    /// invocation is `AM_INTERFACE_MODE=cli am e2e run ...`, and a leaked mode
    /// flips `mcp-agent-mail`/`am` binaries spawned inside a suite onto the
    /// wrong surface (e.g. MCP-mode denial fixtures silently become CLI runs).
    /// Suites that need a mode set it explicitly; direct `bash tests/e2e/...`
    /// invocation never had the variable, and runner invocation must match.
    fn scrub_operator_env(cmd: &mut Command) {
        cmd.env_remove("AM_INTERFACE_MODE");
    }

    fn run_suite_once(&self, suite: &Suite) -> std::io::Result<SuiteExecution> {
        // Build the command
        let mut cmd = Command::new("bash");
        cmd.arg(&suite.script_path);
        cmd.current_dir(&self.config.project_root);
        Self::scrub_operator_env(&mut cmd);

        // Set environment
        cmd.env("E2E_PROJECT_ROOT", &self.config.project_root);
        if self.config.keep_tmp {
            cmd.env("AM_E2E_KEEP_TMP", "1");
        }
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }
        if let Some(root) = &self.config.artifact_dir {
            // The producer owns a fresh directory for each attempt; retries
            // cannot inherit a successful artifact from a failed attempt.
            let suite_root = root.join(&suite.name);
            fs::create_dir_all(&suite_root)?;
            let attempt = tempfile::Builder::new()
                .prefix("attempt-")
                .tempdir_in(&suite_root)?
                .keep();
            cmd.env("AM_E2E_ARTIFACT_DIR", &attempt);
            if self.config.release_scorecard {
                cmd.env("AM_E2E_RELEASE_RECEIPT", attempt.join("receipt.json"));
            }
        }

        self.execute_script(cmd)
    }

    fn execute_script(&self, mut cmd: Command) -> std::io::Result<SuiteExecution> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Suite deadline elapsed before child admission",
            ));
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let result = self.capture_suite_child(&mut child);
        if result.is_err() {
            // A capture/setup error must not abandon the process we launched.
            #[cfg(unix)]
            crate::terminate_child_process_group(child.id(), signal_hook::consts::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    #[cfg(unix)]
    fn capture_suite_child(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<SuiteExecution> {
        let mut stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("missing stdout"))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("missing stderr"))?;
        Self::nonblocking_pipe(&stdout_pipe)?;
        Self::nonblocking_pipe(&stderr_pipe)?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let (mut stdout_done, mut stderr_done) = (false, false);
        let started = Instant::now();
        let mut exited_at = None;
        let mut shutdown_at = None;
        let mut timed_out = false;
        let mut capture_incomplete = false;
        loop {
            if !stdout_done {
                stdout_done =
                    Self::drain_pipe(&mut stdout_pipe, &mut stdout, self.config.max_output_bytes)?;
            }
            if !stderr_done {
                stderr_done =
                    Self::drain_pipe(&mut stderr_pipe, &mut stderr, self.config.max_output_bytes)?;
            }
            let exited = child.try_wait()?.is_some();
            capture_incomplete |= stdout.len() > self.config.max_output_bytes
                || stderr.len() > self.config.max_output_bytes;
            timed_out |= self.command_timed_out(started);
            if exited && stdout_done && stderr_done {
                break;
            }
            if exited {
                exited_at.get_or_insert_with(Instant::now);
            }
            capture_incomplete |=
                exited_at.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1));
            if (timed_out || capture_incomplete) && shutdown_at.is_none() {
                crate::terminate_child_process_group(child.id(), signal_hook::consts::SIGTERM);
                shutdown_at = Some(Instant::now());
            }
            if shutdown_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(35)) {
                crate::terminate_child_process_group(child.id(), signal_hook::consts::SIGKILL);
                let _ = child.kill();
                child.wait()?;
                // A setsid descendant can retain the pipes outside our group.
                // Close our nonblocking readers after the bounded drain; never
                // detach a blocked reader thread or claim complete output.
                stdout_done |=
                    Self::drain_pipe(&mut stdout_pipe, &mut stdout, self.config.max_output_bytes)?;
                stderr_done |=
                    Self::drain_pipe(&mut stderr_pipe, &mut stderr, self.config.max_output_bytes)?;
                capture_incomplete |= !stdout_done || !stderr_done;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        capture_incomplete |= stdout.len() > self.config.max_output_bytes
            || stderr.len() > self.config.max_output_bytes;
        Ok(SuiteExecution {
            output: std::process::Output {
                status: child.wait()?,
                stdout,
                stderr,
            },
            timed_out,
            capture_incomplete,
        })
    }

    #[cfg(unix)]
    fn nonblocking_pipe(pipe: &impl std::os::fd::AsFd) -> std::io::Result<()> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = OFlag::from_bits_retain(fcntl(pipe, FcntlArg::F_GETFL)?);
        fcntl(pipe, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(())
    }

    #[cfg(unix)]
    fn drain_pipe(
        reader: &mut impl std::io::Read,
        output: &mut Vec<u8>,
        limit: usize,
    ) -> std::io::Result<bool> {
        let mut buffer = [0_u8; 8192];
        // Fairness budget: an endless writer must not starve the other stream,
        // child-status checks or timeout enforcement.
        for _ in 0..64 {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    let retain = count.min(limit.saturating_add(1).saturating_sub(output.len()));
                    output.extend_from_slice(&buffer[..retain]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    #[cfg(not(unix))]
    fn capture_suite_child(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<SuiteExecution> {
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("Failed to capture stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("Failed to capture stderr"))?;

        // Drain both streams concurrently, retaining at most limit + 1 bytes.
        // The extra byte distinguishes exact-limit output from lost evidence.
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limit = self.config.max_output_bytes;
        let stdout_overflow = std::sync::Arc::clone(&overflow);
        let stderr_overflow = std::sync::Arc::clone(&overflow);
        let stdout_handle =
            std::thread::spawn(move || Self::capture_bounded(stdout_pipe, limit, &stdout_overflow));
        let stderr_handle =
            std::thread::spawn(move || Self::capture_bounded(stderr_pipe, limit, &stderr_overflow));
        let started = Instant::now();
        let mut timed_out = false;
        let mut capture_incomplete = false;
        let mut exited_at = None;
        let mut shutdown_at = None;
        let mut killed = false;
        loop {
            let exited = child.try_wait()?.is_some();
            timed_out |= self.command_timed_out(started);
            if exited && stdout_handle.is_finished() && stderr_handle.is_finished() {
                break;
            }
            if exited {
                exited_at.get_or_insert_with(Instant::now);
            }
            capture_incomplete |= overflow.load(std::sync::atomic::Ordering::Relaxed)
                || exited_at.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1));
            if (timed_out || capture_incomplete) && shutdown_at.is_none() {
                #[cfg(unix)]
                crate::terminate_child_process_group(child.id(), signal_hook::consts::SIGTERM);
                #[cfg(not(unix))]
                let _ = child.kill();
                shutdown_at = Some(Instant::now());
            }
            if !killed && shutdown_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(35)) {
                #[cfg(unix)]
                crate::terminate_child_process_group(child.id(), signal_hook::consts::SIGKILL);
                let _ = child.kill();
                killed = true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let status = child.wait()?;
        let stdout = stdout_handle
            .join()
            .map_err(|_| std::io::Error::other("suite stdout reader panicked"))??;
        let stderr = stderr_handle
            .join()
            .map_err(|_| std::io::Error::other("suite stderr reader panicked"))??;
        capture_incomplete |= overflow.load(std::sync::atomic::Ordering::Relaxed);
        let output = std::process::Output {
            status,
            stdout,
            stderr,
        };

        Ok(SuiteExecution {
            output,
            timed_out,
            capture_incomplete,
        })
    }

    #[cfg(any(not(unix), test))]
    fn capture_bounded(
        mut reader: impl std::io::Read,
        limit: usize,
        overflow: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let count = match reader.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                return Ok(out);
            }
            let retain = count.min(limit.saturating_add(1).saturating_sub(out.len()));
            out.extend_from_slice(&buffer[..retain]);
            if out.len() > limit {
                overflow.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn is_native_suite(name: &str) -> bool {
        name == Self::NATIVE_HTTP_SUITE
            || name == Self::NATIVE_HTTP_STREAMABLE_SUITE
            || name == Self::NATIVE_MCP_API_PARITY_SUITE
            || name == Self::NATIVE_SHARE_SUITE
            || name == Self::NATIVE_SHARE_VERIFY_LIVE_SUITE
            || name == Self::NATIVE_ARCHIVE_SUITE
            || name == Self::NATIVE_DUAL_MODE_SUITE
            || name == Self::NATIVE_MODE_MATRIX_SUITE
            || name == Self::NATIVE_SECURITY_PRIVACY_SUITE
            || name == Self::NATIVE_TUI_INTERACTION_SUITE
            || name == Self::NATIVE_TUI_INTERACTIONS_SUITE
            || name == Self::NATIVE_TUI_COMPAT_MATRIX_SUITE
            || name == Self::NATIVE_TUI_STARTUP_SUITE
            || name == Self::NATIVE_TUI_A11Y_SUITE
    }

    fn run_native_http_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
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
        cmd.env("AM_HTTP_HARNESS_SUITE", &suite.name);
        cmd.env("AM_E2E_HTTP_REQUIRE_PASS", "1");
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_HTTP_ARTIFACT_DIR", artifact_root);
        }
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn run_native_share_archive_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "share_archive_harness",
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
        cmd.env("AM_SHARE_ARCHIVE_HARNESS_SUITE", &suite.name);
        cmd.env("AM_E2E_SHARE_ARCHIVE_REQUIRE_PASS", "1");
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_SHARE_ARCHIVE_ARTIFACT_DIR", artifact_root);
        }
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn run_native_mode_matrix_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
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
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn run_native_security_privacy_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
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
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn run_native_tui_a11y_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
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
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn run_native_tui_transport_suite(&self, suite: &Suite) -> SuiteResult {
        let mut cmd = Command::new("cargo");
        Self::scrub_operator_env(&mut cmd);
        cmd.args([
            "test",
            "-p",
            "mcp-agent-mail-cli",
            "--test",
            "tui_transport_harness",
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
        cmd.env("AM_TUI_HARNESS_SUITE", &suite.name);
        cmd.env("AM_E2E_TUI_REQUIRE_PASS", "1");
        if let Some(artifact_root) = &self.config.artifact_dir {
            cmd.env("AM_TUI_ARTIFACT_DIR", artifact_root);
        }
        self.execute_native_cargo_suite(suite, cmd)
    }

    fn execute_native_cargo_suite(&self, suite: &Suite, cmd: Command) -> SuiteResult {
        let started_at = Utc::now();
        let start_instant = Instant::now();
        let execution = self.execute_script(cmd);
        let elapsed = start_instant.elapsed();
        let ended_at = Utc::now();

        match execution {
            Ok(execution) => {
                let output = &execution.output;
                let stdout = Self::truncate_output(&output.stdout, self.config.max_output_bytes);
                let mut stderr =
                    Self::truncate_output(&output.stderr, self.config.max_output_bytes);
                let counts = self.native_cargo_counts(output);
                let (assertions_passed, assertions_failed, assertions_skipped) =
                    counts.unwrap_or_default();
                let passed = !execution.timed_out
                    && !execution.capture_incomplete
                    && output.status.success()
                    && assertions_passed > 0
                    && assertions_failed == 0;
                let exit_code = if execution.timed_out {
                    stderr.push_str(
                        "\nNative suite timed out; child output is not terminal evidence",
                    );
                    124
                } else if execution.capture_incomplete {
                    stderr.push_str(
                        "\nNative suite exceeded its capture limit or retained child output pipes",
                    );
                    125
                } else {
                    output.status.code().unwrap_or(-1)
                };
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
            Err(error) => SuiteResult {
                name: suite.name.clone(),
                passed: false,
                exit_code: -1,
                duration_ms: elapsed.as_millis() as u64,
                stdout: String::new(),
                stderr: format!("Failed to execute native {} suite: {error}", suite.name),
                assertions_passed: 0,
                assertions_failed: 1,
                assertions_skipped: 0,
                started_at: started_at.to_rfc3339(),
                ended_at: ended_at.to_rfc3339(),
            },
        }
    }

    fn run_native_dual_mode_suite(&self, suite: &Suite) -> SuiteResult {
        let bounded = Self {
            registry: self.registry.clone(),
            config: self.config.clone(),
            deadline: self
                .config
                .timeout
                .and_then(|limit| Instant::now().checked_add(limit)),
        };
        bounded.run_native_dual_mode_checks(suite)
    }

    fn run_native_dual_mode_checks(&self, suite: &Suite) -> SuiteResult {
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
            let mut cmd = Command::new("cargo");
            cmd.args(["build", "-p", package]);
            cmd.current_dir(&self.config.project_root);
            let output = self
                .execute_complete_command(cmd)
                .map_err(|error| format!("Failed to run cargo build for {package}: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "cargo build -p {package} failed with exit code {:?}: {}",
                    output.status.code(),
                    Self::output_excerpt(&output.stderr, 500)
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
        let mut cmd = Command::new(binary); // ubs:ignore -- Internal dual-mode callers use only the resolved am/mcp-agent-mail artifacts; requests are separate argv.
        cmd.args(args);
        cmd.current_dir(&self.config.project_root);
        Self::scrub_operator_env(&mut cmd);
        for (key, value) in env_map {
            cmd.env(key, value);
        }
        self.execute_complete_command(cmd)
    }

    fn command_timed_out(&self, started: Instant) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            || self
                .config
                .timeout
                .is_some_and(|limit| started.elapsed() >= limit)
    }

    /// Preserve ordinary nonzero exits: a mode-rejection check expects them.
    /// Timeout or incomplete capture must instead fail the check itself, even
    /// when a termination handler exits with the expected rejection code.
    fn execute_complete_command(&self, cmd: Command) -> std::io::Result<std::process::Output> {
        let execution = self.execute_script(cmd)?;
        let failure = if execution.timed_out {
            Some((std::io::ErrorKind::TimedOut, "Command timed out"))
        } else if execution.capture_incomplete {
            Some((std::io::ErrorKind::Other, "Command capture is incomplete"))
        } else {
            None
        };
        if let Some((kind, reason)) = failure {
            return Err(std::io::Error::new(
                kind,
                format!(
                    "{reason}; stdout: {}; stderr: {}",
                    Self::output_excerpt(&execution.output.stdout, 500),
                    Self::output_excerpt(&execution.output.stderr, 500)
                ),
            ));
        }
        Ok(execution.output)
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

    fn native_cargo_counts(&self, output: &std::process::Output) -> Option<(u32, u32, u32)> {
        if output.stdout.len() > self.config.max_output_bytes
            || output.stderr.len() > self.config.max_output_bytes
        {
            return None;
        }
        Self::cargo_test_counts(&String::from_utf8_lossy(&output.stdout))
    }

    /// Native Cargo lanes need terminal libtest counts, not a fabricated
    /// single assertion for any process that exits zero. Ignored tests remain
    /// visible to the release coverage gate.
    fn cargo_test_counts(stdout: &str) -> Option<(u32, u32, u32)> {
        let mut totals = (0_u32, 0_u32, 0_u32);
        let mut summaries = 0;
        for line in stdout.lines() {
            let Some(rest) = line.strip_prefix("test result: ") else {
                continue;
            };
            let successful = rest.starts_with("ok. ");
            let rest = rest
                .strip_prefix("ok. ")
                .or_else(|| rest.strip_prefix("FAILED. "))?;
            let mut fields = rest.split(';');
            let passed = fields
                .next()?
                .trim()
                .strip_suffix(" passed")?
                .parse::<u32>()
                .ok()?;
            let failed = fields
                .next()?
                .trim()
                .strip_suffix(" failed")?
                .parse::<u32>()
                .ok()?;
            let ignored = fields
                .next()?
                .trim()
                .strip_suffix(" ignored")?
                .parse::<u32>()
                .ok()?;
            fields
                .next()?
                .trim()
                .strip_suffix(" measured")?
                .parse::<u32>()
                .ok()?;
            fields
                .next()?
                .trim()
                .strip_suffix(" filtered out")?
                .parse::<u32>()
                .ok()?;
            let duration = fields
                .next()?
                .trim()
                .strip_prefix("finished in ")?
                .strip_suffix('s')?
                .parse::<f64>()
                .ok()?;
            if !duration.is_finite() || duration < 0.0 || fields.next().is_some() {
                return None;
            }
            if successful != (failed == 0) {
                return None;
            }
            totals.0 = totals.0.checked_add(passed)?;
            totals.1 = totals.1.checked_add(failed)?;
            totals.2 = totals.2.checked_add(ignored)?;
            summaries += 1;
        }
        // Every native adapter selects exactly one integration-test binary.
        // Extra summaries are ambiguous nested output, not extra coverage.
        (summaries == 1).then_some(totals)
    }

    /// Truncates output to max bytes.
    fn truncate_output(bytes: &[u8], max_bytes: usize) -> String {
        if bytes.len() <= max_bytes {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            let truncated = String::from_utf8_lossy(&bytes[..max_bytes]);
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

        // Strip ANSI escape codes (compiled once, reused across calls)
        static ANSI_RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"));
        let ansi_regex = &*ANSI_RE;

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
    /// Exact source/executable observation and namespace for this release run.
    pub evidence: Option<ReleaseRunEvidence>,
}

impl RunReport {
    /// Returns true if a nonempty selection completed without a failed or
    /// skipped suite. An empty run is not successful verification.
    #[must_use]
    pub fn success(&self) -> bool {
        self.total > 0 && self.failed == 0 && self.skipped == 0 && self.passed == self.total
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
// Release scorecard (br-bvq1x.14.13 / N13)
// ──────────────────────────────────────────────────────────────────────────────

/// Suite whose per-incident-class scorecard feeds the release scorecard.
pub const INCIDENT_CORPUS_SUITE: &str = "incident_corpus";

/// Identity observed before launching the selected suites. Source inputs and
/// the runner executable are named separately: a runtime observation must not
/// masquerade as a build-system attestation of a different child executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRunEvidence {
    pub run_id: String,
    pub directory: PathBuf,
    pub required_suites: Vec<String>,
    /// Incident ID to family, captured before any producer runs.
    pub required_incident_cases: BTreeMap<String, String>,
    pub runner_executable_sha256: String,
    pub source_inputs: BTreeMap<String, String>,
    pub target: String,
    pub features: Vec<String>,
}

impl ReleaseRunEvidence {
    fn prepare(config: &RunConfig, suites: &[String]) -> std::io::Result<Self> {
        let project = fs::canonicalize(&config.project_root)?;
        let base = config
            .artifact_dir
            .clone()
            .unwrap_or_else(|| project.join("tests/artifacts/release_scorecard"));
        reject_symlinked_path(&base)?;
        fs::create_dir_all(&base)?;
        let directory = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(fs::canonicalize(base)?)?
            .keep();
        let run_id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| std::io::Error::other("invalid release run directory"))?
            .to_string();
        let mut source_inputs = BTreeMap::new();
        for file in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
            source_inputs.insert(file.to_string(), sha256_file(&project.join(file))?);
        }
        let mut required_incident_cases = BTreeMap::new();
        if suites.iter().any(|suite| suite == INCIDENT_CORPUS_SUITE) {
            let manifest_path = project.join("tests/fixtures/corruption_corpus/manifest.json");
            let bytes = fs::read(&manifest_path)?;
            source_inputs.insert(
                "incident_manifest_sha256".to_string(),
                hex::encode(Sha256::digest(&bytes)),
            );
            let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
            let fixtures = manifest["fixtures"]
                .as_array()
                .filter(|items| !items.is_empty())
                .ok_or_else(|| std::io::Error::other("empty incident manifest"))?;
            for fixture in fixtures {
                let id = fixture["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| std::io::Error::other("incident fixture lacks an id"))?;
                if required_incident_cases
                    .insert(id.to_string(), "L1".to_string())
                    .is_some()
                {
                    return Err(std::io::Error::other("duplicate incident fixture id"));
                }
            }
            for (id, family) in [
                ("cli_mcp_name_mismatch_matrix", "L2"),
                ("http_decode_before_tool", "L2"),
                ("fd_exhaustion_resource_busy", "L2"),
                ("mixed_load_write_concurrency_cliff", "L3"),
                ("tui_render_stall_heartbeat", "L3"),
                ("atc_tick_budget_overrun", "L3"),
                ("host_pressure_not_corruption", "EE"),
            ] {
                if required_incident_cases
                    .insert(id.to_string(), family.to_string())
                    .is_some()
                {
                    return Err(std::io::Error::other(
                        "incident fixture collides with a required workflow",
                    ));
                }
            }
        }
        let output = mcp_agent_mail_core::git_cmd::GitCmd::new(&project)
            .args(["rev-parse", "HEAD"])
            .run()?;
        if !output.status.success() {
            return Err(std::io::Error::other(
                "cannot identify release source revision",
            ));
        }
        source_inputs.insert(
            "revision".to_string(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        );
        let output = mcp_agent_mail_core::git_cmd::GitCmd::new(&project)
            .args([
                "diff", "--binary", "HEAD", "--", "crates", "scripts", "tests", ".cargo",
            ])
            .run()?;
        if !output.status.success() {
            return Err(std::io::Error::other(
                "cannot identify release source overlay",
            ));
        }
        source_inputs.insert(
            "overlay_sha256".to_string(),
            hex::encode(Sha256::digest(&output.stdout)),
        );
        let sibling = project.join("../frankensearch-rel-0332");
        let output = mcp_agent_mail_core::git_cmd::GitCmd::new(&sibling)
            .args(["rev-parse", "HEAD"])
            .run()?;
        if !output.status.success() {
            return Err(std::io::Error::other(
                "cannot identify gated frankensearch revision",
            ));
        }
        source_inputs.insert(
            "frankensearch_revision".to_string(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        );
        source_inputs.insert(
            "frankensearch_lock_sha256".to_string(),
            sha256_file(&sibling.join("Cargo.lock"))?,
        );
        Ok(Self {
            run_id,
            directory,
            required_suites: suites.to_vec(),
            required_incident_cases,
            runner_executable_sha256: sha256_file(&std::env::current_exe()?)?,
            source_inputs,
            target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            features: [
                ("default", cfg!(feature = "default")),
                ("portable", cfg!(feature = "portable")),
            ]
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(name, _)| name.to_string())
            .collect(),
        })
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn reject_symlinked_path(path: &Path) -> std::io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::other(
                    "release evidence path contains a symlink",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

const RELEASE_RECEIPT_PREFIX: &str = "AM_E2E_RELEASE_RECEIPT ";

#[derive(Debug, Serialize, Deserialize)]
struct IncidentReceipt {
    schema_version: u32,
    run: ReleaseRunEvidence,
    scorecard_path: PathBuf,
    scorecard_sha256: String,
}

/// Read only the artifact explicitly returned by the terminal producer. A
/// newer file elsewhere, even in the same second, has no authority here.
fn read_incident_scorecard(report: &RunReport) -> Result<(PathBuf, serde_json::Value), String> {
    let run = report.evidence.as_ref().ok_or("missing_run_identity")?;
    let suite = report
        .results
        .iter()
        .find(|r| r.name == INCIDENT_CORPUS_SUITE)
        .ok_or("missing_incident_suite")?;
    if !suite.passed || suite.exit_code != 0 {
        return Err("incident_producer_failed".to_string());
    }
    let mut receipts = suite
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix(RELEASE_RECEIPT_PREFIX));
    let text = receipts.next().ok_or("missing_incident_receipt")?;
    if receipts.next().is_some() {
        return Err("duplicate_incident_receipt".to_string());
    }
    let receipt: IncidentReceipt =
        serde_json::from_str(text).map_err(|error| format!("invalid_incident_receipt: {error}"))?;
    if receipt.schema_version != 1 || &receipt.run != run {
        return Err("incident_run_or_candidate_mismatch".to_string());
    }
    reject_symlinked_path(&receipt.scorecard_path)
        .map_err(|e| format!("unsafe_incident_path: {e}"))?;
    let path = fs::canonicalize(&receipt.scorecard_path)
        .map_err(|e| format!("missing_incident_artifact: {e}"))?;
    if !path.starts_with(run.directory.join(INCIDENT_CORPUS_SUITE)) {
        return Err("incident_artifact_outside_run".to_string());
    }
    let file = fs::File::open(&path).map_err(|e| format!("incident_open: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("incident_metadata: {e}"))?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
        return Err("incident_artifact_not_bounded_regular_file".to_string());
    }
    // Cap the read itself as well: a producer may grow a file after metadata
    // was inspected. Never allocate an unbounded buffer on that path.
    use std::io::Read as _;
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("incident_read: {e}"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("incident_artifact_not_bounded_regular_file".to_string());
    }
    if hex::encode(Sha256::digest(&bytes)) != receipt.scorecard_sha256 {
        return Err("incident_digest_mismatch".to_string());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("incident_json: {e}"))?;
    if value["schema_version"] != 1 || value["suite"] != INCIDENT_CORPUS_SUITE {
        return Err("incident_schema_mismatch".to_string());
    }
    Ok((path, value))
}

/// Result consumed by the CLI exit-status gate as well as the operator.
pub struct ReleaseScorecard {
    pub path: PathBuf,
    pub release_ready: bool,
    pub problems: Vec<String>,
}

/// Writes the aggregated release-readiness scorecard for a completed run.
///
/// Per-suite rows join each `SuiteResult` with the registry's `@tags:` and
/// description metadata (the suite-level anchor). Per-incident-class rows
/// with their originating session-history anchors are lifted from the
/// `scorecard.json` that the `incident_corpus` suite produced during this
/// run. The combined `release_ready` verdict is true only when every suite
/// passed AND its exact incident-class receipt is itself release-ready —
/// a run that omits the incident corpus can never claim release readiness.
///
/// Returns the published path and the verdict used by the CLI exit gate.
pub fn write_release_scorecard(
    report: &RunReport,
    registry: &SuiteRegistry,
    project_root: &Path,
) -> std::io::Result<ReleaseScorecard> {
    let mut problems: Vec<String> = Vec::new();
    if report.total == 0 {
        problems.push("empty_suite_selection".to_string());
    }
    if report.results.len() != report.total as usize {
        problems.push("incomplete_suite_results".to_string());
    }
    let mut names = std::collections::BTreeSet::new();
    for result in &report.results {
        if !names.insert(&result.name) {
            problems.push(format!("duplicate_suite: {}", result.name));
        }
        if registry.get(&result.name).is_none() {
            problems.push(format!("unknown_suite: {}", result.name));
        }
        if !result.passed || result.exit_code != 0 {
            problems.push(format!("suite_not_terminal_success: {}", result.name));
        }
        if result.assertions_passed == 0
            || result.assertions_failed != 0
            || result.assertions_skipped != 0
        {
            problems.push(format!("incomplete_assertion_coverage: {}", result.name));
        }
    }
    if let Some(run) = &report.evidence {
        let required: std::collections::BTreeSet<_> = run.required_suites.iter().collect();
        if required.len() != run.required_suites.len() || required != names {
            problems.push("required_suite_mismatch".to_string());
        }
    } else {
        problems.push("missing_run_identity".to_string());
    }

    let suites: Vec<serde_json::Value> = report
        .results
        .iter()
        .map(|r| {
            let meta = registry.get(&r.name);
            serde_json::json!({
                "name": r.name,
                "passed": r.passed,
                "exit_code": r.exit_code,
                "duration_ms": r.duration_ms,
                "assertions_passed": r.assertions_passed,
                "assertions_failed": r.assertions_failed,
                "assertions_skipped": r.assertions_skipped,
                "description": meta.and_then(|m| m.description.clone()),
                "tags": meta.map(|m| m.tags.clone()).unwrap_or_default(),
            })
        })
        .collect();

    let corpus_ran = report
        .results
        .iter()
        .any(|r| r.name == INCIDENT_CORPUS_SUITE);
    let mut incident_classes = serde_json::Value::Null;
    let mut incident_summary = serde_json::Value::Null;
    let mut incident_source: Option<String> = None;
    let mut incident_fresh = false;
    let mut incident_ready = false;
    if corpus_ran {
        match read_incident_scorecard(report) {
            Ok((path, value)) => {
                incident_fresh = true;
                incident_source = Some(path.display().to_string());
                incident_ready = value
                    .get("release_ready")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !incident_ready {
                    problems.push("incident_not_release_ready".to_string());
                }
                incident_classes = value
                    .get("classes")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                incident_summary = value
                    .get("summary")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let mut classes = std::collections::BTreeSet::new();
                let mut actual_cases = BTreeMap::new();
                if let Some(rows) = incident_classes.as_array().filter(|rows| !rows.is_empty()) {
                    for row in rows {
                        if row["id"].as_str().is_none_or(str::is_empty)
                            || row["family"].as_str().is_none_or(str::is_empty)
                            || !classes.insert((row["family"].as_str(), row["id"].as_str()))
                            || row["status"] != "pass"
                        {
                            problems.push("incomplete_or_duplicate_incident_class".to_string());
                        }
                        if let (Some(id), Some(family)) =
                            (row["id"].as_str(), row["family"].as_str())
                        {
                            actual_cases.insert(id.to_string(), family.to_string());
                        }
                    }
                    if report.evidence.as_ref().is_none_or(|run| {
                        run.required_incident_cases.is_empty()
                            || run.required_incident_cases != actual_cases
                    }) {
                        problems.push("required_incident_case_mismatch".to_string());
                    }
                    if incident_summary["total"].as_u64() != Some(rows.len() as u64)
                        || incident_summary["pass"].as_u64() != Some(rows.len() as u64)
                        || incident_summary["fail"].as_u64() != Some(0)
                        || incident_summary["skip"].as_u64() != Some(0)
                    {
                        problems.push("incident_summary_mismatch".to_string());
                    }
                } else {
                    problems.push("empty_incident_classes".to_string());
                }
            }
            Err(error) => problems.push(error),
        }
    } else {
        problems.push(
            "incident_corpus suite was not part of this run; per-incident-class evidence is missing"
                .to_string(),
        );
    }

    let release_ready =
        report.success() && corpus_ran && incident_fresh && incident_ready && problems.is_empty();

    let scorecard = serde_json::json!({
        "schema_version": 2,
        "kind": "release_scorecard",
        "generated_at": report.ended_at,
        "evidence": report.evidence,
        "run": {
            "started_at": report.started_at,
            "ended_at": report.ended_at,
            "duration_ms": report.duration_ms,
            "suites_total": report.total,
            "suites_passed": report.passed,
            "suites_failed": report.failed,
        },
        "suites": suites,
        "incident_scorecard": {
            "source": incident_source,
            "fresh": incident_fresh,
            "release_ready": incident_ready,
            "summary": incident_summary,
        },
        "incident_classes": incident_classes,
        "problems": problems,
        "release_ready": release_ready,
    });

    let out_dir = if let Some(run) = &report.evidence {
        run.directory.clone()
    } else {
        let base = project_root.join("tests/artifacts/release_scorecard");
        reject_symlinked_path(&base)?;
        fs::create_dir_all(&base)?;
        tempfile::Builder::new()
            .prefix("unverified-")
            .tempdir_in(&base)?
            .keep()
    };
    reject_symlinked_path(&out_dir)?;
    let out_path = out_dir.join("release_scorecard.json");
    // Keep the staging witness. Linking publishes a complete inode atomically
    // and refuses to overwrite any existing report, without deleting a file.
    let stage = out_dir.join("release_scorecard.pending.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    use std::io::Write as _;
    writeln!(file, "{scorecard:#}")?;
    file.sync_all()?;
    fs::hard_link(stage, &out_path)?;
    Ok(ReleaseScorecard {
        path: out_path,
        release_ready,
        problems,
    })
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
            evidence: None,
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
            evidence: None,
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
    fn runner_empty_filter_does_not_execute_unselected_suite() {
        let root = TempDir::new().expect("tempdir").keep();
        write_suite_script(
            &root,
            "available",
            "#!/bin/sh\nprintf ran > unexpected-execution\necho 'Pass: 1  Fail: 0  Skip: 0'\n",
        );
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                ..Default::default()
            },
        )
        .expect("runner");

        for report in [
            runner.run_filtered(Some(&["missing".to_string()]), None, None),
            runner.run_filtered(None, Some(&["*".to_string()]), None),
            runner.run_filtered(None, None, Some(&["absent-tag".to_string()])),
        ] {
            assert_eq!(report.total, 0);
            assert!(report.results.is_empty());
            assert!(!report.success());
            assert_eq!(report.exit_code(), 1);
            assert!(!root.join("unexpected-execution").exists());
        }

        // The unfiltered command must still execute the requested real script.
        let report = runner.run(&[]);
        assert!(report.success());
        assert_eq!(report.results[0].assertions_passed, 1);
        assert_eq!(
            fs::read_to_string(root.join("unexpected-execution")).unwrap(),
            "ran"
        );
    }

    #[test]
    fn shell_exit_zero_does_not_override_missing_or_failed_assertions() {
        let root = TempDir::new().unwrap().keep();
        for (name, summary) in [
            ("missing", "setup complete"),
            ("empty", "Pass: 0 Fail: 0 Skip: 0"),
            ("skipped", "Pass: 0 Fail: 0 Skip: 3"),
            ("failed", "Pass: 3 Fail: 1 Skip: 0"),
            ("positive", "Pass: 3 Fail: 0 Skip: 0"),
        ] {
            write_suite_script(
                &root,
                name,
                &format!("#!/bin/sh\necho '{summary}'\nexit 0\n"),
            );
        }
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        for name in ["missing", "empty", "skipped", "failed", "positive"] {
            let report = runner.run(&[name.to_owned()]);
            assert_eq!(report.success(), name == "positive", "{name}");
            assert_eq!(report.results[0].exit_code, i32::from(name != "positive"));
        }
    }

    #[test]
    fn runner_unknown_suite_is_an_explicit_failure_even_with_a_passing_suite() {
        let root = TempDir::new().expect("tempdir").keep();
        write_suite_script(
            &root,
            "available",
            "#!/bin/sh\necho 'Pass: 1  Fail: 0  Skip: 0'\n",
        );
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                ..Default::default()
            },
        )
        .expect("runner");

        for selection in [
            vec!["missing".to_string()],
            vec!["available".to_string(), "missing".to_string()],
        ] {
            let report = runner.run(&selection);
            assert_eq!(report.total as usize, selection.len());
            assert_eq!(report.failed, 1);
            assert!(!report.success());
            let missing = report.results.last().unwrap();
            assert_eq!(missing.name, "missing");
            assert_eq!(missing.exit_code, 2);
            assert!(missing.stderr.contains("Suite not found"));
        }
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
        let report_include = runner.run_filtered(Some(&include), None, None);
        assert_eq!(report_include.total, 1);
        assert_eq!(report_include.failed, 1);
        assert_eq!(report_include.results[0].name, "fail");

        let exclude = vec!["fail".to_string()];
        let report_exclude = runner.run_filtered(None, Some(&exclude), None);
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
    fn runner_honors_artifact_directory_for_ordinary_runs_and_retries() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(
            &root,
            "artifact",
            r#"#!/usr/bin/env bash
set -eu
printf '%s\n' "$AM_E2E_ARTIFACT_DIR" >> attempts.txt
printf 'owned\n' > "$AM_E2E_ARTIFACT_DIR/witness.txt"
if [ "$(wc -l < attempts.txt)" -eq 1 ]; then exit 1; fi
echo 'Pass: 1  Fail: 0  Skip: 0'
"#,
        );
        let artifacts = root.join("requested-artifacts");
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                artifact_dir: Some(artifacts.clone()),
                retries: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let report = runner.run(&["artifact".to_string()]);
        assert!(report.success());
        let attempts = fs::read_to_string(root.join("attempts.txt")).unwrap();
        let paths: Vec<_> = attempts.lines().map(PathBuf::from).collect();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
        for path in paths {
            assert!(path.starts_with(artifacts.join("artifact")));
            assert_eq!(
                fs::read_to_string(path.join("witness.txt")).unwrap(),
                "owned\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn e2e_remote_required_helpers_reject_missing_artifacts_without_local_build() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = TempDir::new().unwrap().keep();
        let source = include_str!("../../../scripts/e2e_lib.sh");
        let ensure = source
            .split_once("e2e_ensure_binary() {\n")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let compat = source
            .split_once("e2e_sqlite3_compat_bin() {\n")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let stale = root.join("target/debug");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("am"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(stale.join("am"), fs::Permissions::from_mode(0o700)).unwrap();
        for required in ["E2E_CARGO_REQUIRE_RCH", "RCH_REQUIRE_REMOTE"] {
            let script = format!(
                r#"set -eu
e2e_log() {{ echo "$*" >&2; }}
_e2e_build_binary() {{ printf '%s\n' "$E2E_CARGO_FORCE_LOCAL" >> build_modes; }}
cargo() {{ printf unexpected-local-build >> forbidden; return 99; }}
type() {{ return 1; }}
e2e_ensure_binary() {{
{ensure}
}}
e2e_sqlite3_compat_bin() {{
{compat}
}}
if e2e_ensure_binary am; then exit 10; fi
if e2e_sqlite3_compat_bin; then exit 11; fi
test ! -e forbidden
test "$(tail -n 1 build_modes)" = 0
"#,
            );
            let script_path = root.join(format!("{required}.sh"));
            fs::write(&script_path, script).unwrap();
            let output = Command::new("bash")
                .arg(script_path)
                .current_dir(&root)
                .env("E2E_PROJECT_ROOT", &root)
                .env("CARGO_TARGET_DIR", root.join("expected-target"))
                .env("E2E_CARGO_REQUIRE_RCH", "0")
                .env("RCH_REQUIRE_REMOTE", "0")
                .env("E2E_CARGO_FORCE_LOCAL", "0")
                .env("E2E_FORCE_BUILD", "0")
                .env(required, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("remote-required"));
        }
    }

    #[test]
    fn runner_rejects_success_before_output_overflow_and_bounds_capture() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(
            &root,
            "overflow",
            "#!/bin/sh\necho 'Pass: 1  Fail: 0  Skip: 0'\nhead -c 1048576 /dev/zero\n",
        );
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                max_output_bytes: 128,
                ..Default::default()
            },
        )
        .unwrap();
        let report = runner.run(&["overflow".to_string()]);
        let result = &report.results[0];
        assert!(!report.success());
        assert_eq!(result.exit_code, 125);
        assert_eq!(result.assertions_passed, 1);
        assert!(result.stdout.len() < 256);
        assert!(result.stderr.contains("capture limit"));

        let overflow = std::sync::atomic::AtomicBool::new(false);
        let exact = Runner::capture_bounded(&b"12345"[..], 5, &overflow).unwrap();
        assert_eq!(exact, b"12345");
        assert!(!overflow.load(std::sync::atomic::Ordering::Relaxed));
        let excess = Runner::capture_bounded(&b"123456789"[..], 5, &overflow).unwrap();
        assert_eq!(excess, b"123456");
        assert!(overflow.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[cfg(unix)]
    #[test]
    fn runner_timeout_terminates_descendant_holding_output_pipe() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(
            &root,
            "descendant",
            "#!/bin/bash\nsleep 30 &\ntrap 'wait; exit 0' TERM\nwait\n",
        );
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                timeout: Some(Duration::from_millis(100)),
                ..Default::default()
            },
        )
        .unwrap();
        let started = Instant::now();
        let report = runner.run(&["descendant".to_string()]);
        assert_eq!(report.results[0].exit_code, 124);
        assert!(!report.success());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn native_adapters_bound_hangs_and_both_output_streams() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = TempDir::new().unwrap().keep();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("cargo"), r#"#!/bin/sh
printf '%s\n' "$$" > "$CARGO_FIXTURE_PID"
case "$CARGO_FIXTURE_MODE" in
  timeout)
    trap 'echo "test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"; exit 0' TERM
    /bin/sleep 30 &
    wait
    ;;
  stdout) while :; do printf '012345678901234567890123456789012345678901234567890123456789012345\n'; done ;;
  stderr) while :; do printf '012345678901234567890123456789012345678901234567890123456789012345\n' >&2; done ;;
esac
"#).unwrap();
        fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o700)).unwrap();
        for suite in [
            "http",
            "share",
            "mode_matrix",
            "security_privacy",
            "tui_a11y",
            "tui_interaction",
        ] {
            write_suite_script(&root, suite, "#!/bin/sh\nexit 99\n");
        }
        for mode in ["timeout", "stdout", "stderr"] {
            let pid_path = root.join(format!("{mode}.pid"));
            let runner = Runner::new(
                &root,
                RunConfig {
                    project_root: root.clone(),
                    max_output_bytes: 1024,
                    timeout: Some(Duration::from_millis(500)),
                    env: HashMap::from([
                        ("PATH".to_string(), bin.to_string_lossy().into_owned()),
                        ("CARGO_FIXTURE_MODE".to_string(), mode.to_string()),
                        (
                            "CARGO_FIXTURE_PID".to_string(),
                            pid_path.to_string_lossy().into_owned(),
                        ),
                    ]),
                    ..Default::default()
                },
            )
            .unwrap();
            for suite in [
                "http",
                "share",
                "mode_matrix",
                "security_privacy",
                "tui_a11y",
                "tui_interaction",
            ] {
                let started = Instant::now();
                let result = runner.run_suite(runner.registry.get(suite).unwrap());
                let pid: i32 = fs::read_to_string(&pid_path)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert!(!result.passed, "{suite}/{mode}: {}", result.stdout);
                assert_eq!(
                    result.exit_code,
                    if mode == "timeout" { 124 } else { 125 },
                    "{suite}/{mode}: {}",
                    result.stderr
                );
                assert!(started.elapsed() < Duration::from_secs(5), "{suite}/{mode}");
                assert!(result.stdout.len() < 1200 && result.stderr.len() < 1400);
                assert_eq!(
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
                    Err(nix::errno::Errno::ESRCH),
                    "unreaped cargo peer: {suite}/{mode}"
                );
                if mode == "timeout" {
                    // A valid-looking summary emitted by the TERM handler
                    // does not turn a timed-out operation into success.
                    assert_eq!(result.assertions_passed, 7, "{suite}");
                    assert!(result.stderr.contains("timed out"));
                } else {
                    assert!(result.stderr.contains("capture limit"));
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn dual_mode_commands_preserve_exits_and_reject_incomplete_children() {
        let root = TempDir::new().unwrap().keep();
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                max_output_bytes: 1024,
                timeout: Some(Duration::from_millis(500)),
                ..Default::default()
            },
        )
        .unwrap();
        let pid_path = root.join("child.pid");
        let env = HashMap::from([(
            "DUAL_MODE_FIXTURE_PID".to_owned(),
            pid_path.to_string_lossy().into_owned(),
        )]);
        for (script, expected) in [
            ("echo allowed; exit 0", Ok(0)),
            ("echo rejected >&2; exit 2", Ok(2)),
            (
                "trap 'echo rejected >&2; exit 2' TERM; /bin/sleep 30 & wait",
                Err(std::io::ErrorKind::TimedOut),
            ),
            (
                "while :; do echo 012345678901234567890123456789; done",
                Err(std::io::ErrorKind::Other),
            ),
            (
                "while :; do echo 012345678901234567890123456789 >&2; done",
                Err(std::io::ErrorKind::Other),
            ),
        ] {
            let script = format!("printf '%s\\n' \"$$\" > \"$DUAL_MODE_FIXTURE_PID\"; {script}");
            let started = Instant::now();
            let output = runner.run_dual_mode_command(Path::new("/bin/sh"), &["-c", &script], &env);
            match expected {
                Ok(code) => assert_eq!(output.unwrap().status.code(), Some(code)),
                Err(kind) => assert_eq!(output.unwrap_err().kind(), kind),
            }
            let pid: i32 = fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
                Err(nix::errno::Errno::ESRCH),
                "dual-mode child was not reaped"
            );
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn dual_mode_children_share_one_deadline_and_stop_admission() {
        let root = TempDir::new().unwrap().keep();
        let mut runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            },
        )
        .unwrap();
        runner.deadline = Some(Instant::now() + Duration::from_secs(1));
        let env = HashMap::new();
        let first = runner
            .run_dual_mode_command(Path::new("/bin/sh"), &["-c", "echo ready"], &env)
            .unwrap();
        assert!(first.status.success());
        // Spend the suite's remaining budget in a different child. A fresh
        // per-command 30-second timeout would leave this one running.
        let started = Instant::now();
        let second = runner
            .run_dual_mode_command(
                Path::new("/bin/sh"),
                &["-c", "trap 'exit 2' TERM; /bin/sleep 30 & wait"],
                &env,
            )
            .unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let marker = root.join("must-not-launch");
        let env = HashMap::from([(
            "ADMISSION_MARKER".to_owned(),
            marker.to_string_lossy().into_owned(),
        )]);
        let refused = runner
            .run_dual_mode_command(
                Path::new("/bin/sh"),
                &["-c", "echo launched > \"$ADMISSION_MARKER\""],
                &env,
            )
            .unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::TimedOut);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn runner_closes_capture_when_descendant_escapes_process_group() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(
            &root,
            "escaped",
            r#"#!/bin/bash
python3 - "$E2E_PROJECT_ROOT/escaped.pid" <<'PY' &
import os, pathlib, sys, time
os.setsid()
pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))
time.sleep(90)
PY
while [ ! -s "$E2E_PROJECT_ROOT/escaped.pid" ]; do sleep 0.01; done
echo 'Pass: 1  Fail: 0  Skip: 0'
exit 0
"#,
        );
        let runner = Runner::new(
            &root,
            RunConfig {
                project_root: root.clone(),
                timeout: Some(Duration::from_secs(70)),
                ..Default::default()
            },
        )
        .unwrap();
        let started = Instant::now();
        let report = runner.run(&["escaped".to_string()]);
        let elapsed = started.elapsed();
        // This intentionally escaped fixture is owned by this test. The
        // runner cannot reap an unrelated session; stop its exact recorded
        // PID before assertions, even when the regression returns a failure.
        let pid: i32 = fs::read_to_string(root.join("escaped.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let signal_result = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        assert!(
            signal_result.is_ok(),
            "fixture should still hold its output pipes: {signal_result:?}"
        );
        assert!(!report.success());
        assert_eq!(report.results[0].exit_code, 125);
        assert_eq!(report.results[0].assertions_passed, 1);
        assert!(
            report.results[0]
                .stderr
                .contains("retained its output pipes")
        );
        assert!(
            elapsed < Duration::from_secs(45),
            "capture took {elapsed:?}"
        );
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

    #[cfg(unix)]
    #[test]
    fn native_adapters_require_real_terminal_counts_instead_of_exit_zero() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = TempDir::new().unwrap().keep();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        // Controlled process output challenges the adapter contract. Actual
        // mailbox/transport conformance is validated by the real Cargo lanes.
        fs::write(
            bin.join("cargo"),
            "#!/bin/sh\nprintf '%s\\n' \"$CARGO_FIXTURE_OUTPUT\"\nexit \"$CARGO_FIXTURE_EXIT\"\n",
        )
        .unwrap();
        fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o700)).unwrap();
        let suites = [
            "http",
            "share",
            "mode_matrix",
            "security_privacy",
            "tui_a11y",
            "tui_interaction",
        ];
        for suite in suites {
            write_suite_script(&root, suite, "#!/bin/sh\nexit 99\n");
        }
        let good = "test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s";
        for (stdout, exit, passed, counts) in [
            (good.to_string(), "0", true, (7, 0, 0)),
            (good.to_string(), "7", false, (7, 0, 0)),
            ("build succeeded".to_string(), "0", false, (0, 0, 0)),
            (
                "test result: ok. 7 passed; 0 failed; 0 ignored".to_string(),
                "0",
                false,
                (0, 0, 0),
            ),
            (good.replace("7 passed", "0 passed"), "0", false, (0, 0, 0)),
            (format!("{good}\n{good}"), "0", false, (0, 0, 0)),
            (
                format!("{good}\n{}", "x".repeat(256)),
                "0",
                false,
                (0, 0, 0),
            ),
            (good.replace("0 ignored", "2 ignored"), "0", true, (7, 0, 2)),
            (
                good.replace("ok.", "FAILED.")
                    .replace("0 failed", "1 failed"),
                "1",
                false,
                (7, 1, 0),
            ),
        ] {
            let runner = Runner::new(
                &root,
                RunConfig {
                    project_root: root.clone(),
                    max_output_bytes: 128,
                    env: HashMap::from([
                        ("PATH".to_string(), bin.to_string_lossy().into_owned()),
                        ("CARGO_FIXTURE_OUTPUT".to_string(), stdout),
                        ("CARGO_FIXTURE_EXIT".to_string(), exit.to_string()),
                    ]),
                    ..Default::default()
                },
            )
            .unwrap();
            for suite in suites {
                let result = runner.run_suite(runner.registry.get(suite).unwrap());
                assert_eq!(result.passed, passed, "{suite}: {}", result.stderr);
                assert_eq!(
                    (
                        result.assertions_passed,
                        result.assertions_failed,
                        result.assertions_skipped
                    ),
                    counts,
                    "{suite}"
                );
            }
        }
    }

    #[test]
    fn test_run_report_summary_lists_failed_suite_names() {
        let report = RunReport {
            evidence: None,
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
        assert!(Runner::is_native_suite("http_streamable"));
        assert!(Runner::is_native_suite("mcp_api_parity"));
        assert!(Runner::is_native_suite("share"));
        assert!(Runner::is_native_suite("share_verify_live"));
        assert!(Runner::is_native_suite("archive"));
        assert!(Runner::is_native_suite("dual_mode"));
        assert!(Runner::is_native_suite("mode_matrix"));
        assert!(Runner::is_native_suite("security_privacy"));
        assert!(Runner::is_native_suite("tui_interaction"));
        assert!(Runner::is_native_suite("tui_interactions"));
        assert!(Runner::is_native_suite("tui_compat_matrix"));
        assert!(Runner::is_native_suite("tui_startup"));
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

    // ── DurationClass ────────────────────────────────────────────────────

    #[test]
    fn duration_class_as_str_all_variants() {
        assert_eq!(DurationClass::Fast.as_str(), "fast");
        assert_eq!(DurationClass::Normal.as_str(), "normal");
        assert_eq!(DurationClass::Slow.as_str(), "slow");
    }

    #[test]
    fn duration_class_default_is_normal() {
        assert_eq!(DurationClass::default(), DurationClass::Normal);
    }

    #[test]
    fn duration_class_serde_roundtrip() {
        for variant in [
            DurationClass::Fast,
            DurationClass::Normal,
            DurationClass::Slow,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: DurationClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn duration_class_serde_rename_all_lowercase() {
        assert_eq!(
            serde_json::to_string(&DurationClass::Fast).unwrap(),
            "\"fast\""
        );
        assert_eq!(
            serde_json::to_string(&DurationClass::Normal).unwrap(),
            "\"normal\""
        );
        assert_eq!(
            serde_json::to_string(&DurationClass::Slow).unwrap(),
            "\"slow\""
        );
    }

    // ── classify_duration comprehensive ──────────────────────────────────

    #[test]
    fn classify_duration_all_known_slow_suites() {
        let slow_names = [
            "concurrent_agents",
            "crash_restart_test",
            "fault_injection_suite",
            "large_inputs_check",
            "db_corruption_recovery",
            "db_migration_v3",
        ];
        for name in slow_names {
            assert_eq!(
                SuiteRegistry::classify_duration(name, &[]),
                DurationClass::Slow,
                "expected Slow for {name}"
            );
        }
    }

    #[test]
    fn classify_duration_all_known_fast_suites() {
        let fast_names = ["cli_basic", "archive_export", "console_output"];
        for name in fast_names {
            assert_eq!(
                SuiteRegistry::classify_duration(name, &[]),
                DurationClass::Fast,
                "expected Fast for {name}"
            );
        }
    }

    #[test]
    fn classify_duration_unknown_suite_is_normal() {
        assert_eq!(
            SuiteRegistry::classify_duration("http_transport", &[]),
            DurationClass::Normal
        );
    }

    #[test]
    fn classify_duration_slow_tag_overrides_name() {
        // Even a "fast" name becomes Slow with the tag
        assert_eq!(
            SuiteRegistry::classify_duration("cli_fast", &["slow".to_string()]),
            DurationClass::Slow
        );
    }

    // ── matches_pattern edge cases ───────────────────────────────────────

    #[test]
    fn matches_pattern_exact_match() {
        assert!(SuiteRegistry::matches_pattern("guard", "guard"));
    }

    #[test]
    fn matches_pattern_substring_match() {
        assert!(SuiteRegistry::matches_pattern("test_guard_foo", "guard"));
    }

    #[test]
    fn matches_pattern_wildcard_prefix() {
        assert!(SuiteRegistry::matches_pattern("foo_guard", "*guard"));
        assert!(!SuiteRegistry::matches_pattern("guard_foo", "*guard"));
    }

    #[test]
    fn matches_pattern_wildcard_suffix() {
        assert!(SuiteRegistry::matches_pattern("guard_foo", "guard*"));
        assert!(!SuiteRegistry::matches_pattern("foo_guard", "guard*"));
    }

    #[test]
    fn matches_pattern_double_wildcard_matches_substring_glob() {
        assert!(SuiteRegistry::matches_pattern(
            "test_guard_extra",
            "*guard*"
        ));
    }

    #[test]
    fn matches_pattern_multiple_wildcards_match_ordered_segments() {
        assert!(SuiteRegistry::matches_pattern("axbxc", "a*b*c"));
        assert!(!SuiteRegistry::matches_pattern("axbyd", "a*b*c"));
    }

    #[test]
    fn matches_pattern_no_match() {
        assert!(!SuiteRegistry::matches_pattern("http", "guard"));
    }

    #[test]
    fn matches_pattern_empty_name() {
        assert!(!SuiteRegistry::matches_pattern("", "guard"));
    }

    // ── parse_assertions edge cases ──────────────────────────────────────

    #[test]
    fn parse_assertions_empty_string() {
        assert_eq!(Runner::parse_assertions(""), (0, 0, 0));
    }

    #[test]
    fn parse_assertions_no_matching_lines() {
        assert_eq!(
            Runner::parse_assertions("some random output\nnothing useful"),
            (0, 0, 0)
        );
    }

    #[test]
    fn parse_assertions_only_pass() {
        assert_eq!(Runner::parse_assertions("Pass: 10"), (10, 0, 0));
    }

    #[test]
    fn parse_assertions_only_fail() {
        assert_eq!(Runner::parse_assertions("Fail: 3"), (0, 3, 0));
    }

    #[test]
    fn parse_assertions_case_insensitive() {
        assert_eq!(
            Runner::parse_assertions("PASS: 5  FAIL: 2  SKIP: 1"),
            (5, 2, 1)
        );
    }

    #[test]
    fn parse_assertions_mixed_case() {
        assert_eq!(
            Runner::parse_assertions("pass: 8  fail: 0  skip: 3"),
            (8, 0, 3)
        );
    }

    #[test]
    fn parse_assertions_multiline_takes_last_summary() {
        let output = "some output\nPass: 1  Fail: 0\nmore output\nPass: 5  Fail: 2  Skip: 1\n";
        // The last matching line wins because it overwrites
        assert_eq!(Runner::parse_assertions(output), (5, 2, 1));
    }

    #[test]
    fn parse_assertions_ansi_codes_stripped() {
        let output = "\x1b[32mPass: 12\x1b[0m  \x1b[31mFail: 0\x1b[0m";
        assert_eq!(Runner::parse_assertions(output), (12, 0, 0));
    }

    #[test]
    fn parse_assertions_total_prefix_line() {
        let output = "Total: 30  Pass: 27  Fail: 1  Skip: 2";
        assert_eq!(Runner::parse_assertions(output), (27, 1, 2));
    }

    // ── output_excerpt ───────────────────────────────────────────────────

    #[test]
    fn output_excerpt_empty() {
        assert_eq!(Runner::output_excerpt(b"", 100), "");
    }

    #[test]
    fn output_excerpt_short_fits() {
        assert_eq!(Runner::output_excerpt(b"hello", 100), "hello");
    }

    #[test]
    fn output_excerpt_exactly_at_limit() {
        assert_eq!(Runner::output_excerpt(b"12345", 5), "12345");
    }

    #[test]
    fn output_excerpt_over_limit_truncates() {
        let result = Runner::output_excerpt(b"abcdefgh", 5);
        assert_eq!(result, "abcde...");
    }

    // ── truncate_output ──────────────────────────────────────────────────

    #[test]
    fn truncate_output_empty() {
        assert_eq!(Runner::truncate_output(b"", 100), "");
    }

    #[test]
    fn truncate_output_short_fits() {
        assert_eq!(Runner::truncate_output(b"hello world", 100), "hello world");
    }

    #[test]
    fn truncate_output_exactly_at_limit() {
        assert_eq!(Runner::truncate_output(b"12345", 5), "12345");
    }

    #[test]
    fn truncate_output_over_limit() {
        let result = Runner::truncate_output(b"1234567890", 5);
        assert!(result.starts_with("12345"));
        assert!(result.contains("output truncated at 5 bytes"));
    }

    // ── RunConfig default ────────────────────────────────────────────────

    #[test]
    fn run_config_default_values() {
        let cfg = RunConfig::default();
        assert_eq!(cfg.project_root, PathBuf::from("."));
        assert!(cfg.artifact_dir.is_none());
        assert_eq!(cfg.max_output_bytes, 256 * 1024);
        assert_eq!(cfg.timeout, Some(Duration::from_secs(600)));
        assert_eq!(cfg.retries, 0);
        assert!(cfg.env.is_empty());
        assert!(!cfg.parallel);
        assert!(!cfg.keep_tmp);
        assert!(!cfg.force_build);
    }

    // ── SuiteResult serde ────────────────────────────────────────────────

    #[test]
    fn suite_result_serde_roundtrip() {
        let result = SuiteResult {
            name: "guard".to_string(),
            passed: true,
            exit_code: 0,
            duration_ms: 1234,
            stdout: "PASS guard_install".to_string(),
            stderr: String::new(),
            assertions_passed: 5,
            assertions_failed: 0,
            assertions_skipped: 1,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SuiteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "guard");
        assert!(back.passed);
        assert_eq!(back.assertions_passed, 5);
        assert_eq!(back.assertions_skipped, 1);
    }

    // ── RunReport serde + format_summary ─────────────────────────────────

    #[test]
    fn run_report_serde_roundtrip() {
        let report = RunReport {
            evidence: None,
            total: 2,
            passed: 2,
            failed: 0,
            skipped: 0,
            duration_ms: 500,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
            results: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: RunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total, 2);
        assert_eq!(back.passed, 2);
        assert!(back.success());
    }

    #[test]
    fn run_report_format_summary_all_pass() {
        let report = RunReport {
            evidence: None,
            total: 3,
            passed: 3,
            failed: 0,
            skipped: 0,
            duration_ms: 100,
            started_at: "2026-02-12T00:00:00Z".to_string(),
            ended_at: "2026-02-12T00:00:01Z".to_string(),
            results: vec![],
        };
        let summary = report.format_summary();
        assert!(summary.contains("E2E Run: PASS"));
        assert!(!summary.contains("Failed suites:"));
    }

    // ── Suite serde ──────────────────────────────────────────────────────

    #[test]
    fn suite_serde_roundtrip() {
        let suite = Suite {
            name: "alpha".to_string(),
            script_path: PathBuf::from("/tmp/test_alpha.sh"),
            description: Some("Alpha test".to_string()),
            tags: vec!["slow".to_string(), "flaky".to_string()],
            duration_class: DurationClass::Slow,
        };
        let json = serde_json::to_string(&suite).unwrap();
        let back: Suite = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "alpha");
        assert_eq!(back.description.as_deref(), Some("Alpha test"));
        assert_eq!(back.tags, vec!["slow", "flaky"]);
        assert_eq!(back.duration_class, DurationClass::Slow);
    }

    // ── SuiteRegistry edge cases ─────────────────────────────────────────

    #[test]
    fn suite_registry_no_e2e_dir() {
        let temp = TempDir::new().expect("tempdir");
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.suite_names().is_empty());
    }

    #[test]
    fn suite_registry_empty_e2e_dir() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("tests/e2e")).unwrap();
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        assert!(registry.is_empty());
    }

    #[test]
    fn suite_registry_ignores_non_test_files() {
        let temp = TempDir::new().expect("tempdir");
        let e2e = temp.path().join("tests/e2e");
        fs::create_dir_all(&e2e).unwrap();
        // Not matching test_*.sh pattern
        fs::write(e2e.join("helper.sh"), "#!/bin/bash\necho hi").unwrap();
        fs::write(e2e.join("test_foo.py"), "# python").unwrap();
        fs::write(e2e.join("setup_test.sh"), "#!/bin/bash").unwrap();
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        assert!(registry.is_empty());
    }

    #[test]
    fn suite_registry_get_nonexistent() {
        let temp = TempDir::new().expect("tempdir");
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        assert!(registry.get("nonexistent").is_none());
    }

    // ── extract_metadata edge cases ──────────────────────────────────────

    #[test]
    fn extract_metadata_shebang_only() {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("test.sh");
        fs::write(&script, "#!/usr/bin/env bash\n").unwrap();
        let (desc, tags) = SuiteRegistry::extract_metadata(&script);
        assert!(desc.is_none());
        assert!(tags.is_empty());
    }

    #[test]
    fn extract_metadata_skips_e2e_lib_source() {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("test.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\n# source e2e_lib.sh\n# Real description\n",
        )
        .unwrap();
        let (desc, _tags) = SuiteRegistry::extract_metadata(&script);
        assert_eq!(desc.as_deref(), Some("Real description"));
    }

    #[test]
    fn extract_metadata_tags_normalized() {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("test.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\n# @tags: Slow, FLAKY, integration\n",
        )
        .unwrap();
        let (_, tags) = SuiteRegistry::extract_metadata(&script);
        assert_eq!(tags, vec!["slow", "flaky", "integration"]);
    }

    #[test]
    fn extract_metadata_empty_tags_filtered() {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("test.sh");
        fs::write(&script, "#!/usr/bin/env bash\n# @tags: , ,slow,,\n").unwrap();
        let (_, tags) = SuiteRegistry::extract_metadata(&script);
        assert_eq!(tags, vec!["slow"]);
    }

    #[test]
    fn extract_metadata_nonexistent_file() {
        let (desc, tags) = SuiteRegistry::extract_metadata(Path::new("/nonexistent/path"));
        assert!(desc.is_none());
        assert!(tags.is_empty());
    }

    // ── filter combinations ──────────────────────────────────────────────

    #[test]
    fn filter_no_include_no_exclude_returns_all() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(temp.path(), "alpha", "#!/bin/bash\necho ok");
        write_suite_script(temp.path(), "beta", "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        let filtered = registry.filter(None, None, None);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_include_and_exclude_combined() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(temp.path(), "alpha_fast", "#!/bin/bash\necho ok");
        write_suite_script(temp.path(), "alpha_slow", "#!/bin/bash\necho ok");
        write_suite_script(temp.path(), "beta", "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(temp.path()).expect("registry");
        let include = vec!["alpha*".to_string()];
        let exclude = vec!["*slow".to_string()];
        let filtered = registry.filter(Some(&include), Some(&exclude), None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "alpha_fast");
    }

    #[test]
    fn filter_by_tag_selects_only_tagged_suites() {
        let temp = TempDir::new().expect("tempdir");
        write_suite_script(
            temp.path(),
            "tagged",
            "#!/bin/bash\n# Tagged suite\n# @tags: reliability, track-x\necho ok",
        );
        write_suite_script(temp.path(), "untagged", "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(temp.path()).expect("registry");

        let tags = vec!["reliability".to_string()];
        let filtered = registry.filter(None, None, Some(&tags));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "tagged");

        // Tags are exact matches, not substrings/globs.
        let partial = vec!["relia".to_string()];
        assert!(registry.filter(None, None, Some(&partial)).is_empty());

        // Tags compose with exclude.
        let exclude = vec!["tagged".to_string()];
        assert!(
            registry
                .filter(None, Some(&exclude), Some(&tags))
                .is_empty()
        );
    }

    // ── release scorecard (br-bvq1x.14.13) ───────────────────────────────

    fn scorecard_suite_result(name: &str, passed: bool) -> SuiteResult {
        SuiteResult {
            name: name.to_string(),
            passed,
            exit_code: i32::from(!passed),
            duration_ms: 10,
            stdout: String::new(),
            stderr: String::new(),
            assertions_passed: 5,
            assertions_failed: u32::from(!passed),
            assertions_skipped: 0,
            started_at: "2026-02-12T00:00:00+00:00".to_string(),
            ended_at: "2026-02-12T00:00:01+00:00".to_string(),
        }
    }

    fn scorecard_report(results: Vec<SuiteResult>, started_at: &str) -> RunReport {
        let failed = results.iter().filter(|r| !r.passed).count() as u32;
        RunReport {
            evidence: None,
            total: results.len() as u32,
            passed: results.len() as u32 - failed,
            failed,
            skipped: 0,
            duration_ms: 100,
            started_at: started_at.to_string(),
            ended_at: "2026-02-12T00:10:00+00:00".to_string(),
            results,
        }
    }

    // These fixtures exercise the real filesystem/receipt consumer. They do
    // not stand in for the incident corpus or certify a product release.
    fn bind_scorecard_fixture(root: &Path, report: &mut RunReport, value: &serde_json::Value) {
        let directory = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(root)
            .expect("run directory")
            .keep();
        let run = ReleaseRunEvidence {
            run_id: directory.file_name().unwrap().to_str().unwrap().to_string(),
            directory: directory.clone(),
            required_suites: report.results.iter().map(|r| r.name.clone()).collect(),
            required_incident_cases: BTreeMap::from([(
                "zero_byte_wal".to_string(),
                "L1".to_string(),
            )]),
            runner_executable_sha256: sha256_file(&std::env::current_exe().unwrap()).unwrap(),
            source_inputs: BTreeMap::from([(
                "fixture".to_string(),
                "receipt consumer only".to_string(),
            )]),
            target: "test-fixture".to_string(),
            features: Vec::new(),
        };
        let corpus = directory
            .join(INCIDENT_CORPUS_SUITE)
            .join("attempt-fixture");
        fs::create_dir_all(&corpus).unwrap();
        let path = corpus.join("scorecard.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        let receipt = IncidentReceipt {
            schema_version: 1,
            run: run.clone(),
            scorecard_sha256: sha256_file(&path).unwrap(),
            scorecard_path: path,
        };
        if let Some(result) = report
            .results
            .iter_mut()
            .find(|r| r.name == INCIDENT_CORPUS_SUITE)
        {
            result.stdout = format!(
                "{RELEASE_RECEIPT_PREFIX}{}\nPass: 5  Fail: 0  Skip: 0\n",
                serde_json::to_string(&receipt).unwrap()
            );
        }
        report.evidence = Some(run);
    }

    fn passing_incident_fixture() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "suite": "incident_corpus",
            "release_ready": true,
            "classes": [{"family": "L1", "id": "zero_byte_wal", "status": "pass",
                         "anchor": "receipt consumer fixture"}],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skip": 0}
        })
    }

    #[test]
    fn release_scorecard_ready_with_exact_receipt_ignores_newer_foreign_artifact() {
        let root = TempDir::new().expect("tempdir").keep();
        write_suite_script(
            &root,
            INCIDENT_CORPUS_SUITE,
            "#!/bin/bash\n# L4 harness\n# @tags: reliability, corpus\necho ok",
        );
        write_suite_script(
            &root,
            "corruption_taxonomy",
            "#!/bin/bash\n# Track A taxonomy\n# @tags: reliability\necho ok",
        );
        let registry = SuiteRegistry::new(&root).expect("registry");
        let mut report = scorecard_report(
            vec![
                scorecard_suite_result(INCIDENT_CORPUS_SUITE, true),
                scorecard_suite_result("corruption_taxonomy", true),
            ],
            "2026-02-12T00:00:00+00:00",
        );
        bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
        let foreign = root.join("tests/artifacts/incident_corpus/20990101_000000");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("scorecard.json"), r#"{"release_ready":false}"#).unwrap();
        let outcome =
            write_release_scorecard(&report, &registry, &root).expect("write release scorecard");
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&outcome.path).expect("read")).expect("parse");

        assert!(outcome.release_ready);
        assert_eq!(value["kind"], "release_scorecard");
        assert_eq!(value["release_ready"], true);
        assert_eq!(value["problems"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["incident_scorecard"]["fresh"], true);
        assert_eq!(value["incident_classes"][0]["id"], "zero_byte_wal");
        // Suite rows join registry tag metadata.
        let suites = value["suites"].as_array().expect("suites array");
        assert_eq!(suites.len(), 2);
        assert!(suites.iter().all(|s| {
            s["tags"]
                .as_array()
                .expect("tags")
                .iter()
                .any(|t| t == "reliability")
        }));
    }

    #[test]
    fn release_scorecard_not_ready_without_incident_corpus() {
        let root = TempDir::new().expect("tempdir").keep();
        write_suite_script(
            &root,
            "corruption_taxonomy",
            "#!/bin/bash\n# Track A taxonomy\n# @tags: reliability\necho ok",
        );
        let registry = SuiteRegistry::new(&root).expect("registry");

        let mut report = scorecard_report(
            vec![scorecard_suite_result("corruption_taxonomy", true)],
            "2026-02-12T00:00:00+00:00",
        );
        bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
        let outcome =
            write_release_scorecard(&report, &registry, &root).expect("write release scorecard");
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&outcome.path).expect("read")).expect("parse");

        // All suites green, but no incident-class evidence -> never ready.
        assert_eq!(value["release_ready"], false);
        assert!(
            outcome
                .problems
                .iter()
                .any(|problem| problem.contains("not part of this run"))
        );
    }

    #[test]
    fn release_scorecard_not_ready_with_failed_producer_or_wrong_run() {
        let root = TempDir::new().expect("tempdir").keep();
        write_suite_script(
            &root,
            INCIDENT_CORPUS_SUITE,
            "#!/bin/bash\n# L4 harness\n# @tags: reliability\necho ok",
        );
        let registry = SuiteRegistry::new(&root).expect("registry");
        let mut report = scorecard_report(
            vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, false)],
            "2026-02-12T00:00:00+00:00",
        );
        bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
        let outcome = write_release_scorecard(&report, &registry, &root).expect("write");
        assert!(!outcome.release_ready);
        assert!(
            outcome
                .problems
                .iter()
                .any(|p| p == "incident_producer_failed")
        );

        let mut report = scorecard_report(
            vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, true)],
            "2099-01-01T00:00:00+00:00",
        );
        bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
        report
            .evidence
            .as_mut()
            .unwrap()
            .run_id
            .push_str("-different-run");
        let outcome = write_release_scorecard(&report, &registry, &root).expect("write");
        assert!(!outcome.release_ready);
        assert!(
            outcome
                .problems
                .iter()
                .any(|p| p == "incident_run_or_candidate_mismatch")
        );
    }

    #[test]
    fn release_scorecard_rejects_incomplete_coverage_and_candidate_substitution() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(&root, INCIDENT_CORPUS_SUITE, "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(&root).unwrap();
        for defect in [
            "zero",
            "skip",
            "missing_suite",
            "duplicate_suite",
            "candidate",
            "features",
            "target",
            "missing_receipt",
            "duplicate_receipt",
            "truncated_receipt",
            "empty_classes",
            "duplicate_class",
            "unknown_class",
            "failed_class",
            "wrong_summary",
        ] {
            let mut report = scorecard_report(
                vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, true)],
                "2026-09-04T00:00:00Z",
            );
            let mut value = passing_incident_fixture();
            match defect {
                "empty_classes" => value["classes"] = serde_json::json!([]),
                "duplicate_class" => {
                    let duplicate = value["classes"][0].clone();
                    value["classes"].as_array_mut().unwrap().push(duplicate);
                }
                "failed_class" => value["classes"][0]["status"] = "fail".into(),
                "unknown_class" => value["classes"][0]["id"] = "not-in-the-required-set".into(),
                "wrong_summary" => value["summary"]["total"] = 20.into(),
                _ => {}
            }
            bind_scorecard_fixture(&root, &mut report, &value);
            match defect {
                "zero" => report.results[0].assertions_passed = 0,
                "skip" => report.results[0].assertions_skipped = 1,
                "missing_suite" => report
                    .evidence
                    .as_mut()
                    .unwrap()
                    .required_suites
                    .push("http".into()),
                "duplicate_suite" => {
                    report.results.push(report.results[0].clone());
                    report.total += 1;
                    report.passed += 1;
                }
                "candidate" => {
                    report.evidence.as_mut().unwrap().runner_executable_sha256 =
                        "another ELF".into()
                }
                "features" => report
                    .evidence
                    .as_mut()
                    .unwrap()
                    .features
                    .push("portable".into()),
                "target" => report.evidence.as_mut().unwrap().target = "another target".into(),
                "missing_receipt" => report.results[0].stdout.clear(),
                "duplicate_receipt" => {
                    let duplicate = report.results[0].stdout.clone();
                    report.results[0].stdout.push_str(&duplicate);
                }
                "truncated_receipt" => {
                    report.results[0].stdout = format!("{RELEASE_RECEIPT_PREFIX}{{")
                }
                _ => {}
            }
            let outcome = write_release_scorecard(&report, &registry, &root).unwrap();
            assert!(!outcome.release_ready, "accepted {defect}");
            assert!(!outcome.problems.is_empty(), "unexplained {defect}");
        }
    }

    #[test]
    fn release_scorecard_rejects_changed_bytes_and_never_clobbers_publication() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(&root, INCIDENT_CORPUS_SUITE, "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(&root).unwrap();
        let mut report = scorecard_report(
            vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, true)],
            "2026-09-04T00:00:00Z",
        );
        bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
        let (path, _) = read_incident_scorecard(&report).unwrap();
        // Simulate replacement after the terminal receipt was issued.
        fs::write(path, b"{\"release_ready\":true}").unwrap();
        let outcome = write_release_scorecard(&report, &registry, &root).unwrap();
        assert!(!outcome.release_ready);
        assert!(
            outcome
                .problems
                .iter()
                .any(|p| p == "incident_digest_mismatch")
        );
        let original = fs::read(&outcome.path).unwrap();
        assert!(write_release_scorecard(&report, &registry, &root).is_err());
        assert_eq!(fs::read(&outcome.path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn release_scorecard_rejects_symlink_and_outside_run_paths() {
        let root = TempDir::new().unwrap().keep();
        for symlink in [false, true] {
            let mut report = scorecard_report(
                vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, true)],
                "2026-09-04T00:00:00Z",
            );
            bind_scorecard_fixture(&root, &mut report, &passing_incident_fixture());
            let (path, _) = read_incident_scorecard(&report).unwrap();
            let foreign = tempfile::Builder::new()
                .prefix("foreign-")
                .tempdir_in(&root)
                .unwrap()
                .keep();
            let replacement = foreign.join("scorecard.json");
            if symlink {
                std::os::unix::fs::symlink(&path, &replacement).unwrap();
            } else {
                fs::copy(&path, &replacement).unwrap();
            }
            let receipt = IncidentReceipt {
                schema_version: 1,
                run: report.evidence.clone().unwrap(),
                scorecard_sha256: sha256_file(&replacement).unwrap(),
                scorecard_path: replacement,
            };
            report.results[0].stdout = format!(
                "{RELEASE_RECEIPT_PREFIX}{}",
                serde_json::to_string(&receipt).unwrap()
            );
            let error = read_incident_scorecard(&report).unwrap_err();
            assert!(
                error.starts_with(if symlink {
                    "unsafe_incident_path"
                } else {
                    "incident_artifact_outside_run"
                }),
                "{error}"
            );
        }
    }

    #[test]
    fn release_scorecard_concurrent_opposite_verdicts_stay_with_their_runs() {
        let root = TempDir::new().unwrap().keep();
        write_suite_script(&root, INCIDENT_CORPUS_SUITE, "#!/bin/bash\necho ok");
        let registry = SuiteRegistry::new(&root).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let workers: Vec<_> = [false, true]
                .into_iter()
                .map(|ready| {
                    let root = &root;
                    let registry = &registry;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut report = scorecard_report(
                            vec![scorecard_suite_result(INCIDENT_CORPUS_SUITE, true)],
                            "2026-09-04T00:00:00Z",
                        );
                        let mut value = passing_incident_fixture();
                        value["release_ready"] = ready.into();
                        bind_scorecard_fixture(root, &mut report, &value);
                        barrier.wait();
                        let outcome = write_release_scorecard(&report, registry, root).unwrap();
                        assert_eq!(outcome.release_ready, ready);
                        outcome.path
                    })
                })
                .collect();
            let paths: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            assert_ne!(paths[0], paths[1]);
        });
    }

    #[test]
    fn release_scorecard_actual_incident_producer_publishes_terminal_receipt() {
        let root = TempDir::new().unwrap().keep();
        let script = include_str!("../../../tests/e2e/test_incident_corpus.sh");
        let producer = script
            .split_once("if python3 - \"${SCORECARD_ROWS}\"")
            .unwrap()
            .1
            .split_once("<<'PY'\n")
            .unwrap()
            .1
            .split_once("\nPY\n")
            .unwrap()
            .0;
        let manifest = root.join("manifest.json");
        fs::write(
            &manifest,
            r#"{"corpus_id":"producer-contract-test","fixtures":[{"id":"zero_byte_wal"}]}"#,
        )
        .unwrap();
        for skip in [false, true] {
            let run_dir = tempfile::Builder::new()
                .prefix("producer-")
                .tempdir_in(&root)
                .unwrap()
                .keep();
            let rows = run_dir.join("rows.tsv");
            let mut contents = "L1\tzero_byte_wal\tfixture\tpass\tfixture\tfixture\n".to_string();
            for id in [
                "cli_mcp_name_mismatch_matrix",
                "http_decode_before_tool",
                "fd_exhaustion_resource_busy",
                "mixed_load_write_concurrency_cliff",
                "tui_render_stall_heartbeat",
                "atc_tick_budget_overrun",
                "host_pressure_not_corruption",
            ] {
                let status = if skip && id == "atc_tick_budget_overrun" {
                    "skip"
                } else {
                    "pass"
                };
                contents.push_str(&format!("L2\t{id}\tfixture\t{status}\tfixture\tfixture\n"));
            }
            fs::write(&rows, contents).unwrap();
            let scorecard = run_dir.join("scorecard.json");
            let receipt = run_dir.join("receipt.json");
            let producer_path = run_dir.join("producer.py");
            fs::write(&producer_path, producer).unwrap();
            let output = Command::new("python3")
                .arg(producer_path)
                .arg(&rows)
                .arg(&manifest)
                .arg(&scorecard)
                .args(["test-fixture", ""])
                .env(
                    "AM_E2E_RELEASE_RUN",
                    r#"{"run_id":"producer-contract-test"}"#,
                )
                .env("AM_E2E_RELEASE_RECEIPT", &receipt)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&scorecard).unwrap()).unwrap();
            assert_eq!(value["release_ready"], !skip);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
            assert_eq!(value["scorecard_sha256"], sha256_file(&scorecard).unwrap());
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.starts_with(RELEASE_RECEIPT_PREFIX))
            );
        }
    }

    #[test]
    fn release_scorecard_incident_fixture_requires_terminal_cargo_success() {
        let root = TempDir::new().unwrap().keep();
        let script = include_str!("../../../tests/e2e/test_incident_corpus.sh");
        let body = script
            .split_once("run_cargo_fixture() {\n")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        for (exit, count, expected) in [(0, 5, true), (7, 5, false), (0, 0, false)] {
            // Fault injection at the cargo subprocess boundary checks the
            // actual shell validator; these are not product conformance rows.
            let command = format!(
                "e2e_run_cargo() {{ printf 'test result: ok. {count} passed; 0 failed;\\n'; return {exit}; }}\nrun_cargo_fixture() {{\n{body}\n}}\nrun_cargo_fixture case-{exit}-{count}.log 1"
            );
            let script_path = root.join(format!("case-{exit}-{count}.sh"));
            fs::write(&script_path, command).unwrap();
            let output = Command::new("bash")
                .arg(script_path)
                .env("E2E_ARTIFACT_DIR", &root)
                .output()
                .unwrap();
            assert_eq!(output.status.success(), expected, "{exit}/{count}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn workflow_driver_orders_handshake_and_rejects_incomplete_children() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = TempDir::new().unwrap().keep();
        let source = include_str!("../../../tests/e2e/test_workflow_happy_path.sh");
        let driver = source
            .split_once("send_jsonrpc_session() {\n")
            .unwrap()
            .1
            .split_once("<<'PY'\n")
            .unwrap()
            .1
            .split_once("\nPY\n")
            .unwrap()
            .0;
        let binary = root.join("protocol_fixture.py");
        // An adversarial protocol peer checks this driver's ordering and
        // lifecycle. Product conformance still requires the actual am binary.
        fs::write(
            &binary,
            r#"#!/usr/bin/env python3
import json, os, sys, time
initialized = False
mode = os.environ['WORKFLOW_DRIVER_MODE']
for line in sys.stdin:
    req = json.loads(line)
    if req['method'] == 'notifications/initialized':
        initialized = True
        continue
    if req['method'] != 'initialize' and not initialized:
        sys.exit(9)
    if req['method'] != 'initialize':
        if mode == 'eof': sys.exit(0)
        if mode == 'hang': time.sleep(60)
    response_id = 999 if mode == 'wrong_id' else req['id']
    print(json.dumps({'jsonrpc':'2.0','id':response_id,'result':{}}), flush=True)
sys.exit(7 if mode == 'unclean' else 0)
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        for mode in ["complete", "eof", "wrong_id", "unclean", "hang"] {
            let work = root.join(mode);
            fs::create_dir(&work).unwrap();
            let timeout = if mode == "hang" { "0.2" } else { "10" };
            let driver_path = work.join("driver.py");
            fs::write(&driver_path, driver).unwrap();
            let output = Command::new("python3")
                .arg(driver_path)
                .arg(work.join("mailbox.sqlite3"))
                .arg(work.join("archive"))
                .arg(&work)
                .arg(&binary)
                .arg(timeout)
                .args([
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}"#,
                ])
                .env("WORKFLOW_DRIVER_MODE", mode)
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                mode == "complete",
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let records: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let terminal = &records.last().unwrap()["workflow_session"];
            assert_eq!(terminal["passed"], mode == "complete", "{mode}");
            if mode == "complete" {
                assert_eq!(terminal["completed_ids"], serde_json::json!([1, 2]));
            }
        }
    }

    #[test]
    fn workflow_response_validator_requires_reply_and_terminal_session() {
        let root = TempDir::new().unwrap().keep();
        let source = include_str!("../../../tests/e2e/test_workflow_happy_path.sh");
        let body = source
            .split_once("is_error_result() {\n")
            .unwrap()
            .1
            .split_once("\n}\n")
            .unwrap()
            .0;
        let command = format!("is_error_result() {{\n{body}\n}}\nis_error_result \"$1\" 2");
        let script_path = root.join("validator.sh");
        fs::write(&script_path, command).unwrap();
        let reply = r#"{"jsonrpc":"2.0","id":2,"result":{}}"#;
        let terminal = r#"{"workflow_session":{"passed":true,"completed_ids":[1,2]}}"#;
        for (payload, expected) in [
            (format!("{reply}\n{terminal}"), "false"),
            (reply.to_string(), "true"),
            (terminal.to_string(), "true"),
            (format!("{reply}\n{reply}\n{terminal}"), "true"),
            (
                format!("{reply}\n{{\"workflow_session\":{{\"passed\":false}}}}"),
                "true",
            ),
            (format!("{reply}\n{{\"workflow_session\":null}}"), "true"),
            (
                format!(
                    "{reply}\n{{\"workflow_session\":{{\"passed\":true,\"completed_ids\":\"12\"}}}}"
                ),
                "true",
            ),
            (String::new(), "true"),
        ] {
            let output = Command::new("bash")
                .arg(&script_path)
                .arg(&payload)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }

    // ── write_dual_mode_step_artifact: pass case ─────────────────────────

    #[test]
    fn write_dual_mode_step_artifact_pass_no_failure_file() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("dm");
        fs::create_dir_all(root.join("steps")).expect("steps dir");
        fs::create_dir_all(root.join("failures")).expect("failures dir");

        let artifact_root = Some(root.clone());
        let mut step_index = 0usize;
        Runner::write_dual_mode_step_artifact(
            &artifact_root,
            &mut step_index,
            "am",
            "migrate --help",
            "cli",
            "allow",
            0,
            "usage: ...",
            "",
            true, // passed
        );

        assert_eq!(step_index, 1);
        assert!(root.join("steps/step_001.json").exists());
        assert!(!root.join("failures/fail_001.json").exists());

        let step: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join("steps/step_001.json")).unwrap())
                .unwrap();
        assert_eq!(step["passed"], true);
        assert_eq!(step["actual_exit_code"], 0);
    }

    #[test]
    fn write_dual_mode_step_artifact_none_root_is_noop() {
        let mut step_index = 0usize;
        Runner::write_dual_mode_step_artifact(
            &None,
            &mut step_index,
            "am",
            "cmd",
            "cli",
            "allow",
            0,
            "",
            "",
            true,
        );
        // step_index not incremented when root is None
        assert_eq!(step_index, 0);
    }

    // ── RunReport exit_code ──────────────────────────────────────────────

    #[test]
    fn run_report_exit_code_zero_on_success() {
        let r = RunReport {
            evidence: None,
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            duration_ms: 0,
            started_at: String::new(),
            ended_at: String::new(),
            results: vec![],
        };
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn run_report_exit_code_one_on_failure() {
        let r = RunReport {
            evidence: None,
            total: 2,
            passed: 1,
            failed: 1,
            skipped: 0,
            duration_ms: 0,
            started_at: String::new(),
            ended_at: String::new(),
            results: vec![],
        };
        assert_eq!(r.exit_code(), 1);
    }

    // ── native suite constants ───────────────────────────────────────────

    #[test]
    fn native_suite_constants_match_is_native_suite() {
        assert_eq!(Runner::NATIVE_HTTP_SUITE, "http");
        assert_eq!(Runner::NATIVE_HTTP_STREAMABLE_SUITE, "http_streamable");
        assert_eq!(Runner::NATIVE_MCP_API_PARITY_SUITE, "mcp_api_parity");
        assert_eq!(Runner::NATIVE_SHARE_SUITE, "share");
        assert_eq!(Runner::NATIVE_SHARE_VERIFY_LIVE_SUITE, "share_verify_live");
        assert_eq!(Runner::NATIVE_ARCHIVE_SUITE, "archive");
        assert_eq!(Runner::NATIVE_DUAL_MODE_SUITE, "dual_mode");
        assert_eq!(Runner::NATIVE_MODE_MATRIX_SUITE, "mode_matrix");
        assert_eq!(Runner::NATIVE_SECURITY_PRIVACY_SUITE, "security_privacy");
        assert_eq!(Runner::NATIVE_TUI_INTERACTION_SUITE, "tui_interaction");
        assert_eq!(Runner::NATIVE_TUI_INTERACTIONS_SUITE, "tui_interactions");
        assert_eq!(Runner::NATIVE_TUI_COMPAT_MATRIX_SUITE, "tui_compat_matrix");
        assert_eq!(Runner::NATIVE_TUI_STARTUP_SUITE, "tui_startup");
        assert_eq!(Runner::NATIVE_TUI_A11Y_SUITE, "tui_a11y");
    }

    #[test]
    fn is_native_suite_prefix_not_matched() {
        // "http_extra" is NOT a native suite (exact match only)
        assert!(!Runner::is_native_suite("http_extra"));
        assert!(!Runner::is_native_suite("mcp_api_parity_v2"));
        assert!(!Runner::is_native_suite("share_plus"));
        assert!(!Runner::is_native_suite("archive_legacy"));
        assert!(!Runner::is_native_suite("dual_mode_v2"));
        assert!(!Runner::is_native_suite("tui_interaction_extra"));
        assert!(!Runner::is_native_suite(""));
    }
}
