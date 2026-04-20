//! Ephemeral-root classification and default-mailbox safety policy.
//!
//! This module provides the authoritative classification of project roots
//! and execution contexts as ephemeral or production. It implements the
//! 4-tier classification defined in `docs/SPEC-ephemeral-root-policy.md`.
//!
//! # Tiers
//!
//! - **Tier 1 (`SystemTemp`)**: `/tmp`, `/var/tmp`, `/dev/shm`, `$TMPDIR`
//! - **Tier 2 (`TestRepro`)**: test harnesses, repro dirs, `AM_TEST_MODE`
//! - **Tier 3 (`NtmSwarm`)**: NTM/swarm session directories
//! - **Tier 4 (`CiCd`)**: CI/CD environments (`CI=true`, GitHub Actions, etc.)
//!
//! # Classification
//!
//! - `Ephemeral`: at least one high-confidence signal detected
//! - `LikelyEphemeral`: medium-confidence signals only
//! - `Production`: no ephemeral signals detected
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_core::ephemeral::{classify_ephemeral, std_env_lookup};
//!
//! let (class, signals) = classify_ephemeral(project_root, &std_env_lookup);
//! if class.is_ephemeral() {
//!     // Auto-isolate or reject
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::path::Path;

// ============================================================================
// Ephemeral tier
// ============================================================================

/// Tier of ephemeral classification, ordered by specificity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EphemeralTier {
    /// Tier 1: System temporary directories (`/tmp`, `/var/tmp`, `/dev/shm`, `$TMPDIR`).
    SystemTemp,
    /// Tier 2: Test/repro contexts (test harnesses, repro dirs, `AM_TEST_MODE`).
    TestRepro,
    /// Tier 3: NTM/swarm session directories.
    NtmSwarm,
    /// Tier 4: CI/CD environments (`CI=true`, GitHub Actions, etc.).
    CiCd,
}

impl EphemeralTier {
    /// Human-readable label.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SystemTemp => "system_temp",
            Self::TestRepro => "test_repro",
            Self::NtmSwarm => "ntm_swarm",
            Self::CiCd => "ci_cd",
        }
    }
}

impl std::fmt::Display for EphemeralTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Ephemeral class
// ============================================================================

/// Classification of a project's ephemeral status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EphemeralClass {
    /// Definitely ephemeral (at least one high-confidence signal).
    Ephemeral,
    /// Probably ephemeral (medium-confidence signals only).
    LikelyEphemeral,
    /// No ephemeral signals detected.
    Production,
}

impl EphemeralClass {
    /// Whether this classification indicates an ephemeral context.
    #[must_use]
    pub const fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Ephemeral | Self::LikelyEphemeral)
    }

    /// Whether this classification indicates a production context.
    #[must_use]
    pub const fn is_production(&self) -> bool {
        matches!(self, Self::Production)
    }

    /// Human-readable label.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::LikelyEphemeral => "likely_ephemeral",
            Self::Production => "production",
        }
    }
}

impl std::fmt::Display for EphemeralClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Ephemeral mode (config)
// ============================================================================

/// Operator-configurable ephemeral mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EphemeralMode {
    /// Auto-detect ephemeral contexts and isolate them.
    #[default]
    Auto,
    /// Force ephemeral isolation regardless of detection.
    Force,
    /// Never isolate; treat all contexts as production.
    Deny,
}

impl EphemeralMode {
    /// Parse from a string value (case-insensitive).
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "force" | "always" | "true" | "1" => Self::Force,
            "deny" | "never" | "false" | "0" => Self::Deny,
            _ => Self::Auto,
        }
    }

    /// Human-readable label.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Force => "force",
            Self::Deny => "deny",
        }
    }
}

impl std::fmt::Display for EphemeralMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Ephemeral signals
// ============================================================================

/// Individual signals that contributed to ephemeral classification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct EphemeralSignals {
    // ---- Tier 1: System temp (path-based) ----
    /// Path starts with `/tmp` or `/tmp/`.
    pub path_tmp: bool,
    /// Path starts with `/var/tmp` or `/var/tmp/`.
    pub path_var_tmp: bool,
    /// Path starts with `/dev/shm` or `/dev/shm/`.
    pub path_dev_shm: bool,
    /// Path starts with `$TMPDIR`.
    pub path_tmpdir: bool,
    /// Path starts with `/private/tmp` (macOS).
    pub path_private_tmp: bool,
    /// Path starts with `/var/folders` (macOS).
    pub path_var_folders: bool,

    // ---- Tier 2: Test/repro (path + env) ----
    /// Path component contains `test_`, `test-`, `_test`, or `-test`.
    pub path_contains_test: bool,
    /// Path component contains `repro-`, `reproduction/`, or `repro_`.
    pub path_contains_repro: bool,
    /// Path component contains `.tmp` or `.temp`.
    pub path_contains_dot_tmp: bool,
    /// `AM_TEST_MODE=true` (or `=1`) set in environment.
    pub env_am_test_mode: bool,
    /// `RUST_TEST_THREADS` set in environment.
    pub env_rust_test_threads: bool,

    // ---- Tier 3: NTM/swarm ----
    /// Path contains `.ntm/` directory.
    pub path_ntm_dir: bool,
    /// Path matches `ntm-session-*` pattern.
    pub path_ntm_session: bool,
    /// `NTM_SESSION_DIR` or `NTM_SESSION` env var set.
    pub env_ntm: bool,

    // ---- Tier 4: CI/CD ----
    /// `CI=true` set in environment.
    pub env_ci: bool,
    /// `GITHUB_ACTIONS=true` set in environment.
    pub env_github_actions: bool,
    /// Path starts with `/home/runner/work/` (GitHub Actions runner).
    pub path_github_runner: bool,
}

impl EphemeralSignals {
    /// Whether any high-confidence signal is active.
    #[must_use]
    pub const fn has_high_confidence(&self) -> bool {
        // Tier 1 path signals are all high confidence
        self.path_tmp
            || self.path_var_tmp
            || self.path_dev_shm
            || self.path_tmpdir
            || self.path_private_tmp
            || self.path_var_folders
            // Tier 2 high confidence
            || self.env_am_test_mode
            || self.env_rust_test_threads
            // Tier 3 high confidence
            || self.env_ntm
            // Tier 4 high confidence
            || self.env_ci
            || self.env_github_actions
            || self.path_github_runner
    }

    /// Whether any medium-confidence signal is active.
    #[must_use]
    pub const fn has_medium_confidence(&self) -> bool {
        self.path_contains_test
            || self.path_contains_repro
            || self.path_contains_dot_tmp
            || self.path_ntm_dir
            || self.path_ntm_session
    }

    /// Whether any signal is active.
    #[must_use]
    pub const fn has_any(&self) -> bool {
        self.has_high_confidence() || self.has_medium_confidence()
    }

    /// Return the highest-priority tier, if any signals are active.
    #[must_use]
    pub const fn primary_tier(&self) -> Option<EphemeralTier> {
        if self.path_tmp
            || self.path_var_tmp
            || self.path_dev_shm
            || self.path_tmpdir
            || self.path_private_tmp
            || self.path_var_folders
        {
            return Some(EphemeralTier::SystemTemp);
        }
        if self.env_ci || self.env_github_actions || self.path_github_runner {
            return Some(EphemeralTier::CiCd);
        }
        if self.env_ntm || self.path_ntm_dir || self.path_ntm_session {
            return Some(EphemeralTier::NtmSwarm);
        }
        if self.path_contains_test
            || self.path_contains_repro
            || self.path_contains_dot_tmp
            || self.env_am_test_mode
            || self.env_rust_test_threads
        {
            return Some(EphemeralTier::TestRepro);
        }
        None
    }

    /// Collect active signal names (for diagnostics/logging).
    #[must_use]
    pub fn active_signal_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.path_tmp {
            names.push("path_tmp");
        }
        if self.path_var_tmp {
            names.push("path_var_tmp");
        }
        if self.path_dev_shm {
            names.push("path_dev_shm");
        }
        if self.path_tmpdir {
            names.push("path_tmpdir");
        }
        if self.path_private_tmp {
            names.push("path_private_tmp");
        }
        if self.path_var_folders {
            names.push("path_var_folders");
        }
        if self.path_contains_test {
            names.push("path_contains_test");
        }
        if self.path_contains_repro {
            names.push("path_contains_repro");
        }
        if self.path_contains_dot_tmp {
            names.push("path_contains_dot_tmp");
        }
        if self.env_am_test_mode {
            names.push("env_am_test_mode");
        }
        if self.env_rust_test_threads {
            names.push("env_rust_test_threads");
        }
        if self.path_ntm_dir {
            names.push("path_ntm_dir");
        }
        if self.path_ntm_session {
            names.push("path_ntm_session");
        }
        if self.env_ntm {
            names.push("env_ntm");
        }
        if self.env_ci {
            names.push("env_ci");
        }
        if self.env_github_actions {
            names.push("env_github_actions");
        }
        if self.path_github_runner {
            names.push("path_github_runner");
        }
        names
    }
}

// ============================================================================
// Environment lookup helper
// ============================================================================

fn has_nonempty_env_value(env: &impl Fn(&str) -> Option<String>, key: &str) -> bool {
    env(key).is_some_and(|value| !value.trim().is_empty())
}

/// Standard process-environment lookup.
///
/// This intentionally excludes user-global and project-local dotenv layers, but
/// does honor the core process-env test override hook so deterministic tests
/// and harnesses can control ephemeral detection.
#[must_use]
pub fn std_env_lookup(key: &str) -> Option<String> {
    crate::config::process_env_value(key)
}

// ============================================================================
// Path-based detection helpers
// ============================================================================

/// Canonical set of system temp prefixes (Tier 1).
const SYSTEM_TEMP_PREFIXES: &[&str] = &[
    "/tmp",
    "/var/tmp",
    "/dev/shm",
    "/private/tmp",
    "/var/folders",
];

/// Check whether a normalized path string starts with a system temp prefix.
fn starts_with_system_temp(normalized: &str) -> bool {
    for prefix in SYSTEM_TEMP_PREFIXES {
        if normalized == *prefix || normalized.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    false
}

fn has_test_pattern(component: &str) -> bool {
    component.starts_with("test_")
        || component.starts_with("test-")
        || component.ends_with("_test")
        || component.ends_with("-test")
        || component == "tests"
}

fn has_repro_pattern(component: &str) -> bool {
    component.starts_with("repro-")
        || component.starts_with("repro_")
        || component == "reproduction"
}

fn path_has_dot_tmp_component(normalized: &str) -> bool {
    normalized.split('/').any(|comp| {
        comp.starts_with(".tmp") || comp.starts_with(".temp") || comp == ".tmp" || comp == ".temp"
    })
}

/// Check for GitHub Actions runner path (Tier 4).
fn path_is_github_runner(normalized: &str) -> bool {
    normalized.starts_with("/home/runner/work/") || normalized.starts_with("/home/runner/work")
}

// ============================================================================
// Core classification
// ============================================================================

/// Classify a project root as ephemeral or production.
///
/// This is the canonical entry point for ephemeral detection. It checks
/// path-based signals and environment variables to determine the
/// classification and the signals that contributed to it.
///
/// # Arguments
///
/// * `project_root` - Absolute path to the project root directory
/// * `env` - Environment variable lookup function (use `std_env_lookup` for real, or a closure for tests)
///
/// # Returns
///
/// A tuple of `(EphemeralClass, EphemeralSignals)`.
pub fn classify_ephemeral(
    project_root: &Path,
    env: &impl Fn(&str) -> Option<String>,
) -> (EphemeralClass, EphemeralSignals) {
    let mut signals = EphemeralSignals::default();

    // Resolve symlinks so that a project at `/data/projects/foo` which is a
    // symlink to `/tmp/test-foo/` is correctly detected as ephemeral.
    // Falls back to the raw path if canonicalization fails (e.g. broken symlink).
    let resolved =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let normalized = resolved.to_string_lossy().replace('\\', "/");

    // ---- Tier 1: System temp (path-based) ----
    signals.path_tmp = normalized == "/tmp" || normalized.starts_with("/tmp/");
    signals.path_var_tmp = normalized == "/var/tmp" || normalized.starts_with("/var/tmp/");
    signals.path_dev_shm = normalized == "/dev/shm" || normalized.starts_with("/dev/shm/");
    signals.path_private_tmp =
        normalized == "/private/tmp" || normalized.starts_with("/private/tmp/");
    signals.path_var_folders =
        normalized == "/var/folders" || normalized.starts_with("/var/folders/");

    // Also check std::env::temp_dir() at runtime
    let temp_dir = std::env::temp_dir();
    if project_root.starts_with(&temp_dir) && !signals.path_tmp && !signals.path_var_tmp {
        signals.path_tmpdir = true;
    }

    // ---- Tier 2: Test/repro (path + env) ----
    signals.path_contains_test = normalized.split('/').any(has_test_pattern);
    signals.path_contains_repro = normalized.split('/').any(has_repro_pattern);
    signals.path_contains_dot_tmp = path_has_dot_tmp_component(&normalized);

    signals.env_am_test_mode = env("AM_TEST_MODE")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
    signals.env_rust_test_threads = has_nonempty_env_value(env, "RUST_TEST_THREADS");

    // ---- Tier 3: NTM/swarm ----
    signals.path_ntm_dir = normalized.contains("/.ntm/");
    signals.path_ntm_session = normalized.contains("/ntm-session-");
    signals.env_ntm = has_nonempty_env_value(env, "NTM_SESSION_DIR")
        || has_nonempty_env_value(env, "NTM_SESSION");

    // ---- Tier 4: CI/CD ----
    signals.env_ci =
        env("CI").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
    signals.env_github_actions = env("GITHUB_ACTIONS")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
    signals.path_github_runner = path_is_github_runner(&normalized);

    // ---- Compute class ----
    let class = if signals.has_high_confidence() {
        EphemeralClass::Ephemeral
    } else if signals.has_medium_confidence() {
        EphemeralClass::LikelyEphemeral
    } else {
        EphemeralClass::Production
    };

    (class, signals)
}

/// Check whether a path has an ephemeral root (Tier 1 only).
///
/// This is a faster check for the common case where only system temp
/// directories need to be detected. It is equivalent to the Tier 1
/// portion of `classify_ephemeral`, without environment checks.
#[must_use]
pub fn path_has_ephemeral_root(path: &Path) -> bool {
    // Resolve symlinks so `/data/projects/foo` → `/tmp/test/` is detected.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let temp_root = std::env::temp_dir();
    if resolved.starts_with(&temp_root) {
        return true;
    }

    let normalized = resolved.to_string_lossy().replace('\\', "/");
    starts_with_system_temp(&normalized)
}

/// Resolve the effective ephemeral class for a project, considering
/// the operator's `EphemeralMode` configuration.
///
/// - `Force` → always `Ephemeral`
/// - `Deny` → always `Production`
/// - `Auto` → use detected class
#[must_use]
pub const fn resolve_ephemeral_class(
    mode: EphemeralMode,
    detected: EphemeralClass,
) -> EphemeralClass {
    match mode {
        EphemeralMode::Force => EphemeralClass::Ephemeral,
        EphemeralMode::Deny => EphemeralClass::Production,
        EphemeralMode::Auto => detected,
    }
}

// ============================================================================
// Narrow "test-fixture leak" predicate (B2/B3 — archive pollution guard)
// ============================================================================
//
// Unlike `classify_ephemeral` — which is broad and drives the ephemeral
// storage reroute — `is_ephemeral_project_path` is intentionally narrow:
// it matches only path segments whose *shape* screams "this was created by
// a test harness". The goal is to catch leaks from call sites that bypass
// the reroute (direct `queries::ensure_project` calls, benchmarks, etc.)
// and end up writing into the real user archive.
//
// Patterns are drawn directly from the pollution incident of 2026-04-19,
// where 1576+ test-fixture project roots had accumulated in the production
// archive. All four patterns below are represented in that incident.

/// Does a path segment look like a `tempfile.mkdtemp()` / `tempfile::TempDir`
/// random suffix?
///
/// Both Python `tempfile.mkdtemp()` and Rust's `tempfile::TempDir` produce
/// directory names of the form `tmp` + 6-10 lowercase alphanumerics,
/// optionally followed by `-`-prefixed metadata. Examples:
/// `tmp0jdv7c`, `tmpa0tiqx-project-root`, `tmp23baj9-project-root`.
fn looks_like_mkdtemp_segment(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix("tmp") else {
        return false;
    };
    let alnum_run: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let len = alnum_run.len();
    if !(6..=10).contains(&len) {
        return false;
    }
    // mkdtemp uses lowercase-ish alphanumerics; reject obvious mixed-case words
    // like "tmpOverride" or "tmpFixtureConfig" that aren't actually tempdirs.
    if !alnum_run
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }
    let tail = &rest[alnum_run.len()..];
    tail.is_empty() || tail.starts_with('-')
}

/// Does a path segment look like a Go `t.TempDir()` / `TestXxx` directory?
///
/// Go's `testing` package names subtest tempdirs `<TestName><timestamp>`,
/// producing segments like `TestInitCmd_NoArgs2809542922` or
/// `TestHandleListMailProjects_Branch2592788348`.
fn looks_like_go_test_segment(segment: &str) -> bool {
    if !segment.starts_with("Test") {
        return false;
    }
    let rest = &segment[4..];
    let first = rest.chars().next();
    if !matches!(first, Some(c) if c.is_ascii_uppercase()) {
        return false;
    }
    if !rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    // Trailing digit run ≥ 6 is the timestamp signature.
    let digit_suffix_len = rest
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .count();
    digit_suffix_len >= 6
}

/// Returns `true` if `human_key` contains a path segment that matches a
/// known test-fixture naming pattern (`tempfile.mkdtemp`, `t.TempDir()`,
/// `.tmpXXXXXX`). Narrower than `classify_ephemeral`: this is specifically
/// the "refuse to write into the real archive" check, not the
/// "reroute to isolated storage" check.
///
/// Does not canonicalize; operates on the given string so `/tmp/foo` is
/// **not** rejected (that's a valid ephemeral project that should be
/// rerouted, not rejected).
#[must_use]
pub fn is_ephemeral_project_path(human_key: &str) -> bool {
    let normalized = human_key.replace('\\', "/");
    for segment in normalized.split('/') {
        if segment.is_empty() {
            continue;
        }
        if looks_like_mkdtemp_segment(segment) {
            return true;
        }
        if looks_like_go_test_segment(segment) {
            return true;
        }
        // `.tmpXXXXXX` — older Rust tempfile style + some CI runners.
        if let Some(rest) = segment.strip_prefix(".tmp")
            && rest.len() >= 6
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return true;
        }
    }
    false
}

/// Returns `true` when the operator has opted in to allow ephemeral
/// project roots to be registered into the archive. Honors the standard
/// `process_env_value` harness so tests and integration harnesses can
/// override it without touching the real process env.
///
/// Accepts the same truthy vocabulary as [`crate::config::parse_bool`]
/// (`1`/`true`/`t`/`yes`/`y`, case-insensitive) for consistency.
#[must_use]
pub fn ephemeral_project_roots_allowed() -> bool {
    crate::config::process_env_value("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS")
        .map(|v| crate::config::parse_bool(&v, false))
        .unwrap_or(false)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: no env vars set.
    fn empty_env(_key: &str) -> Option<String> {
        None
    }

    /// Helper: returns specific env vars.
    fn env_with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    // ---- EphemeralClass ----

    #[test]
    fn ephemeral_class_is_ephemeral() {
        assert!(EphemeralClass::Ephemeral.is_ephemeral());
        assert!(EphemeralClass::LikelyEphemeral.is_ephemeral());
        assert!(!EphemeralClass::Production.is_ephemeral());
    }

    #[test]
    fn ephemeral_class_is_production() {
        assert!(EphemeralClass::Production.is_production());
        assert!(!EphemeralClass::Ephemeral.is_production());
        assert!(!EphemeralClass::LikelyEphemeral.is_production());
    }

    #[test]
    fn ephemeral_class_serialization() {
        let json = serde_json::to_string(&EphemeralClass::LikelyEphemeral).unwrap();
        assert_eq!(json, "\"likely_ephemeral\"");
        let parsed: EphemeralClass = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, EphemeralClass::LikelyEphemeral);
    }

    // ---- EphemeralTier ----

    #[test]
    fn ephemeral_tier_display() {
        assert_eq!(EphemeralTier::SystemTemp.to_string(), "system_temp");
        assert_eq!(EphemeralTier::TestRepro.to_string(), "test_repro");
        assert_eq!(EphemeralTier::NtmSwarm.to_string(), "ntm_swarm");
        assert_eq!(EphemeralTier::CiCd.to_string(), "ci_cd");
    }

    #[test]
    fn ephemeral_tier_serialization() {
        let json = serde_json::to_string(&EphemeralTier::CiCd).unwrap();
        assert_eq!(json, "\"ci_cd\"");
        let parsed: EphemeralTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, EphemeralTier::CiCd);
    }

    // ---- EphemeralMode ----

    #[test]
    fn ephemeral_mode_from_str_lossy() {
        assert_eq!(EphemeralMode::from_str_lossy("auto"), EphemeralMode::Auto);
        assert_eq!(EphemeralMode::from_str_lossy("force"), EphemeralMode::Force);
        assert_eq!(
            EphemeralMode::from_str_lossy("always"),
            EphemeralMode::Force
        );
        assert_eq!(EphemeralMode::from_str_lossy("true"), EphemeralMode::Force);
        assert_eq!(EphemeralMode::from_str_lossy("1"), EphemeralMode::Force);
        assert_eq!(EphemeralMode::from_str_lossy("deny"), EphemeralMode::Deny);
        assert_eq!(EphemeralMode::from_str_lossy("never"), EphemeralMode::Deny);
        assert_eq!(EphemeralMode::from_str_lossy("false"), EphemeralMode::Deny);
        assert_eq!(EphemeralMode::from_str_lossy("0"), EphemeralMode::Deny);
        assert_eq!(
            EphemeralMode::from_str_lossy("garbage"),
            EphemeralMode::Auto
        );
        assert_eq!(EphemeralMode::from_str_lossy(""), EphemeralMode::Auto);
    }

    #[test]
    fn ephemeral_mode_default_is_auto() {
        assert_eq!(EphemeralMode::default(), EphemeralMode::Auto);
    }

    #[test]
    fn std_env_lookup_honors_process_env_test_overrides() {
        crate::config::with_process_env_overrides_for_test(&[("RUST_TEST_THREADS", "7")], || {
            assert_eq!(std_env_lookup("RUST_TEST_THREADS").as_deref(), Some("7"));
        });
    }

    // ---- Tier 1: System temp paths ----

    #[test]
    fn classify_tmp_path_as_ephemeral() {
        let (class, signals) = classify_ephemeral(Path::new("/tmp/test-project"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_tmp);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::SystemTemp));
    }

    #[test]
    fn classify_var_tmp_as_ephemeral() {
        let (class, signals) = classify_ephemeral(Path::new("/var/tmp/my-project"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_var_tmp);
    }

    #[test]
    fn classify_dev_shm_as_ephemeral() {
        let (class, signals) = classify_ephemeral(Path::new("/dev/shm/agent-session"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_dev_shm);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::SystemTemp));
    }

    #[test]
    fn classify_private_tmp_as_ephemeral() {
        let (class, signals) =
            classify_ephemeral(Path::new("/private/tmp/macos-project"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_private_tmp);
    }

    #[test]
    fn classify_var_folders_as_ephemeral() {
        let (class, signals) =
            classify_ephemeral(Path::new("/var/folders/ab/cd1234/T/test"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_var_folders);
    }

    // ---- Tier 2: Test/repro ----

    #[test]
    fn classify_test_dir_as_likely_ephemeral() {
        let (class, signals) = classify_ephemeral(
            Path::new("/home/user/test-reproduction/project"),
            &empty_env,
        );
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_contains_test);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::TestRepro));
    }

    #[test]
    fn classify_repro_dir_as_likely_ephemeral() {
        let (class, signals) =
            classify_ephemeral(Path::new("/home/user/repro-issue-42/project"), &empty_env);
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_contains_repro);
    }

    #[test]
    fn classify_am_test_mode_as_ephemeral() {
        let env = env_with(&[("AM_TEST_MODE", "true")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.env_am_test_mode);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::TestRepro));
    }

    #[test]
    fn classify_rust_test_threads_as_ephemeral() {
        let env = env_with(&[("RUST_TEST_THREADS", "4")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.env_rust_test_threads);
    }

    #[test]
    fn classify_blank_rust_test_threads_as_production() {
        let env = env_with(&[("RUST_TEST_THREADS", "")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Production);
        assert!(!signals.env_rust_test_threads);
    }

    // ---- Tier 3: NTM/swarm ----

    #[test]
    fn classify_ntm_dir_as_likely_ephemeral() {
        let (class, signals) = classify_ephemeral(
            Path::new("/data/projects/my-project/.ntm/session-abc"),
            &empty_env,
        );
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_ntm_dir);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::NtmSwarm));
    }

    #[test]
    fn classify_ntm_session_env_as_ephemeral() {
        let env = env_with(&[("NTM_SESSION_DIR", "/tmp/ntm-session-123")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.env_ntm);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::NtmSwarm));
    }

    #[test]
    fn classify_blank_ntm_session_env_as_production() {
        let env = env_with(&[("NTM_SESSION_DIR", "")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Production);
        assert!(!signals.env_ntm);
    }

    #[test]
    fn classify_ntm_session_path_as_likely_ephemeral() {
        let (class, signals) =
            classify_ephemeral(Path::new("/data/ntm-session-xyz/project"), &empty_env);
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_ntm_session);
    }

    // ---- Tier 4: CI/CD ----

    #[test]
    fn classify_ci_env_as_ephemeral() {
        let env = env_with(&[("CI", "true")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.env_ci);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::CiCd));
    }

    #[test]
    fn classify_github_actions_as_ephemeral() {
        let env = env_with(&[("GITHUB_ACTIONS", "true")]);
        let (class, signals) = classify_ephemeral(Path::new("/data/projects/real-project"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.env_github_actions);
    }

    #[test]
    fn classify_github_runner_path_as_ephemeral() {
        let (class, signals) =
            classify_ephemeral(Path::new("/home/runner/work/my-repo/my-repo"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_github_runner);
    }

    // ---- Production ----

    #[test]
    fn classify_normal_path_as_production() {
        let (class, signals) =
            classify_ephemeral(Path::new("/data/projects/my-project"), &empty_env);
        assert_eq!(class, EphemeralClass::Production);
        assert!(!signals.has_any());
        assert_eq!(signals.primary_tier(), None);
    }

    #[test]
    fn classify_home_dir_as_production() {
        let (class, _signals) =
            classify_ephemeral(Path::new("/home/ubuntu/projects/my-project"), &empty_env);
        assert_eq!(class, EphemeralClass::Production);
    }

    // ---- Signal aggregation ----

    #[test]
    fn multiple_signals_produce_highest_tier() {
        // Path in /tmp (Tier 1) + CI env (Tier 4) → SystemTemp wins
        let env = env_with(&[("CI", "true")]);
        let (class, signals) = classify_ephemeral(Path::new("/tmp/ci-build"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_tmp);
        assert!(signals.env_ci);
        assert_eq!(signals.primary_tier(), Some(EphemeralTier::SystemTemp));
    }

    #[test]
    fn active_signal_names_lists_all_active() {
        let env = env_with(&[("CI", "true"), ("RUST_TEST_THREADS", "2")]);
        let (_, signals) = classify_ephemeral(Path::new("/tmp/test-project"), &env);
        let names = signals.active_signal_names();
        assert!(names.contains(&"path_tmp"));
        assert!(names.contains(&"env_ci"));
        assert!(names.contains(&"env_rust_test_threads"));
        assert!(names.contains(&"path_contains_test"));
    }

    // ---- resolve_ephemeral_class ----

    #[test]
    fn resolve_force_always_ephemeral() {
        assert_eq!(
            resolve_ephemeral_class(EphemeralMode::Force, EphemeralClass::Production),
            EphemeralClass::Ephemeral
        );
    }

    #[test]
    fn resolve_deny_always_production() {
        assert_eq!(
            resolve_ephemeral_class(EphemeralMode::Deny, EphemeralClass::Ephemeral),
            EphemeralClass::Production
        );
    }

    #[test]
    fn resolve_auto_passes_through() {
        assert_eq!(
            resolve_ephemeral_class(EphemeralMode::Auto, EphemeralClass::LikelyEphemeral),
            EphemeralClass::LikelyEphemeral
        );
        assert_eq!(
            resolve_ephemeral_class(EphemeralMode::Auto, EphemeralClass::Production),
            EphemeralClass::Production
        );
    }

    // ---- path_has_ephemeral_root (fast path) ----

    #[test]
    fn path_has_ephemeral_root_detects_dev_shm() {
        assert!(path_has_ephemeral_root(Path::new("/dev/shm/test")));
        assert!(path_has_ephemeral_root(Path::new("/dev/shm")));
    }

    #[test]
    fn path_has_ephemeral_root_detects_tmp() {
        assert!(path_has_ephemeral_root(Path::new("/tmp/project")));
        assert!(path_has_ephemeral_root(Path::new("/var/tmp/project")));
        assert!(path_has_ephemeral_root(Path::new("/private/tmp/macos")));
        assert!(path_has_ephemeral_root(Path::new("/var/folders/ab/cd")));
    }

    #[test]
    fn path_has_ephemeral_root_rejects_normal_paths() {
        assert!(!path_has_ephemeral_root(Path::new("/data/projects/x")));
        assert!(!path_has_ephemeral_root(Path::new("/home/ubuntu/x")));
    }

    // ---- Edge cases ----

    #[test]
    fn path_bare_tmp_classified() {
        let (class, signals) = classify_ephemeral(Path::new("/tmp"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_tmp);
    }

    #[test]
    fn path_bare_dev_shm_classified() {
        let (class, signals) = classify_ephemeral(Path::new("/dev/shm"), &empty_env);
        assert_eq!(class, EphemeralClass::Ephemeral);
        assert!(signals.path_dev_shm);
    }

    #[test]
    fn empty_path_is_production() {
        let (class, _) = classify_ephemeral(Path::new(""), &empty_env);
        assert_eq!(class, EphemeralClass::Production);
    }

    #[test]
    fn dot_tmp_in_path_component() {
        let (class, signals) =
            classify_ephemeral(Path::new("/data/.tmp/scratch-project"), &empty_env);
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_contains_dot_tmp);
    }

    #[test]
    fn am_test_mode_various_values() {
        // "1" should work
        let env = env_with(&[("AM_TEST_MODE", "1")]);
        let (class, _) = classify_ephemeral(Path::new("/data/proj"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);

        // "yes" should work
        let env = env_with(&[("AM_TEST_MODE", "yes")]);
        let (class, _) = classify_ephemeral(Path::new("/data/proj"), &env);
        assert_eq!(class, EphemeralClass::Ephemeral);

        // "0" should NOT trigger
        let env = env_with(&[("AM_TEST_MODE", "0")]);
        let (class, _) = classify_ephemeral(Path::new("/data/proj"), &env);
        assert_eq!(class, EphemeralClass::Production);
    }

    #[test]
    fn path_containing_tests_directory() {
        let (class, signals) = classify_ephemeral(
            Path::new("/data/projects/my-app/tests/fixtures"),
            &empty_env,
        );
        assert_eq!(class, EphemeralClass::LikelyEphemeral);
        assert!(signals.path_contains_test);
    }

    // ────────────────────────────────────────────────────────────────
    // is_ephemeral_project_path — the narrow pollution-guard predicate.
    // Rejects the specific test-fixture naming patterns observed in the
    // 2026-04-19 archive pollution incident (1500+ leaked project roots).
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn is_ephemeral_project_path_matches_python_tempfile_mkdtemp_pattern() {
        // Observed: tmp0jdv7c-project-root, tmp0rhizz-project-root, ...
        assert!(is_ephemeral_project_path(
            "/data/tmp/tmp0jdv7c-project-root"
        ));
        assert!(is_ephemeral_project_path(
            "/data/tmp/tmpa0tiqx-project-root"
        ));
        assert!(is_ephemeral_project_path(
            "/data/tmp/tmp23baj9-project-root"
        ));
        // Bare mkdtemp directory without our suffix
        assert!(is_ephemeral_project_path("/tmp/tmpabcdef"));
    }

    #[test]
    fn is_ephemeral_project_path_matches_go_testing_tmpdir_pattern() {
        // Observed: data-tmp-ntm-go-tmp-testinitcmd-noargs2809542922-001
        // which came from: /data/tmp/ntm-go/tmp/TestInitCmd_NoArgs2809542922/001
        assert!(is_ephemeral_project_path(
            "/data/tmp/ntm-go/tmp/TestInitCmd_NoArgs2809542922/001"
        ));
        assert!(is_ephemeral_project_path(
            "/var/tmp/TestHandleListMailProjects_Branch2592788348/002"
        ));
    }

    #[test]
    fn is_ephemeral_project_path_rejects_plain_temp_paths() {
        // Plain /tmp/foo is a legitimate ephemeral project that should be
        // *rerouted* by compute_ephemeral_storage_root, NOT rejected here.
        assert!(!is_ephemeral_project_path("/tmp/foo"));
        assert!(!is_ephemeral_project_path("/tmp"));
        assert!(!is_ephemeral_project_path("/var/tmp/shared-work"));
    }

    #[test]
    fn is_ephemeral_project_path_rejects_production_paths() {
        assert!(!is_ephemeral_project_path("/data/projects/backend"));
        assert!(!is_ephemeral_project_path("/home/user/work/my-app"));
        assert!(!is_ephemeral_project_path(
            "/data/projects/mcp_agent_mail_rust"
        ));
        // Paths that merely *contain* the substring "tmp" shouldn't match
        assert!(!is_ephemeral_project_path("/home/user/tmp-workspace"));
        assert!(!is_ephemeral_project_path("/data/projects/my-tmpfs-cache"));
    }

    #[test]
    fn is_ephemeral_project_path_matches_dot_tmp_style() {
        // `.tmpXXXXXX` is the older Rust tempfile naming, still seen in some CI.
        assert!(is_ephemeral_project_path("/data/work/.tmpAbC123/project"));
    }

    #[test]
    fn is_ephemeral_project_path_tolerates_mixed_case_words() {
        // `tmpOverride`, `tmpFixtureConfig` are legitimate code names — not mkdtemp.
        assert!(!is_ephemeral_project_path("/data/projects/tmpOverride"));
        assert!(!is_ephemeral_project_path(
            "/data/projects/backend/src/tmpFixtureConfig"
        ));
    }

    #[test]
    fn is_ephemeral_project_path_handles_windows_separators() {
        // Backslashes are normalized to forward slashes before scanning.
        assert!(is_ephemeral_project_path(
            r"C:\Users\runner\tmpabc123-project-root"
        ));
        assert!(!is_ephemeral_project_path(r"C:\work\my-project"));
    }

    // ────────────────────────────────────────────────────────────────
    // ephemeral_project_roots_allowed — env-override escape hatch.
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn ephemeral_project_roots_allowed_honors_env_override() {
        // Verify both the "off by default" and "on when set" semantics
        // using the explicit test env override so we don't depend on
        // whatever the process env happens to contain.
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "")],
            || assert!(!ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "true")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "yes")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        // Short variants supported by parse_bool
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "t")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "y")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        // Case-insensitive
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "TRUE")],
            || assert!(ephemeral_project_roots_allowed()),
        );
        // Non-affirmative values don't count
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "0")],
            || assert!(!ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "false")],
            || assert!(!ephemeral_project_roots_allowed()),
        );
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "no")],
            || assert!(!ephemeral_project_roots_allowed()),
        );
        // Unknown values default to false.
        crate::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "maybe")],
            || assert!(!ephemeral_project_roots_allowed()),
        );
    }
}
