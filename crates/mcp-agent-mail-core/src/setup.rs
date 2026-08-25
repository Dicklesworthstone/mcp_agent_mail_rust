//! Native agent discovery and MCP configuration for `am setup`.
//!
//! Contains agent-agnostic logic: agent registry, config format definitions,
//! token management, JSON merge, atomic file writes. Lives in core (not cli)
//! so it can be reused by the server or tests.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors that can occur during setup operations.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("expected JSON object at top level or servers key")]
    NotJsonObject,

    #[error("unknown agent platform: {0}")]
    UnknownPlatform(String),

    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Agent Platform
// ---------------------------------------------------------------------------

/// Which coding agent platform we're configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPlatform {
    Claude,
    Codex,
    Cursor,
    Gemini,
    /// Oh My Pi (`omp`) coding agent.
    Omp,
    /// Antigravity (`agy`) — Google's successor to the retired Gemini CLI.
    /// Reads MCP servers from `~/.gemini/config/mcp_config.json` (distinct from
    /// Gemini's `~/.gemini/settings.json`), verified empirically against the
    /// live agy 1.0.7 binary.
    Antigravity,
    OpenCode,
    FactoryDroid,
    Cline,
    Windsurf,
    GithubCopilot,
}

impl AgentPlatform {
    /// All supported platforms.
    pub const ALL: &[Self] = &[
        Self::Claude,
        Self::Codex,
        Self::Cursor,
        Self::Gemini,
        Self::Omp,
        Self::Antigravity,
        Self::OpenCode,
        Self::FactoryDroid,
        Self::Cline,
        Self::Windsurf,
        Self::GithubCopilot,
    ];

    /// Map from agent-detect slug to platform.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" | "codex-cli" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            "gemini" | "gemini-cli" => Some(Self::Gemini),
            "omp" | "oh-my-pi" => Some(Self::Omp),
            "antigravity" | "agy" | "antigravity-cli" => Some(Self::Antigravity),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "factory" | "factory-droid" => Some(Self::FactoryDroid),
            "cline" => Some(Self::Cline),
            "windsurf" => Some(Self::Windsurf),
            "github-copilot" | "copilot" => Some(Self::GithubCopilot),
            _ => None,
        }
    }

    /// Canonical slug for this platform.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::Omp => "omp",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::FactoryDroid => "factory",
            Self::Cline => "cline",
            Self::Windsurf => "windsurf",
            Self::GithubCopilot => "github-copilot",
        }
    }

    /// Human-readable display name.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::Omp => "Oh My Pi (OMP)",
            Self::Antigravity => "Antigravity (agy)",
            Self::OpenCode => "OpenCode",
            Self::FactoryDroid => "Factory Droid",
            Self::Cline => "Cline",
            Self::Windsurf => "Windsurf",
            Self::GithubCopilot => "GitHub Copilot",
        }
    }

    /// Project-relative config files this platform writes into `project_dir`
    /// that may embed the bearer token (security issue #148: these MUST be
    /// covered by the auto-generated `.gitignore` so `git add -A` never commits
    /// a live credential). User-level files (e.g. `~/.codex/config.toml`,
    /// `~/.claude.json`) are not project-tracked and excluded here.
    #[must_use]
    pub const fn project_local_secret_files(self) -> &'static [&'static str] {
        match self {
            // Neither Claude nor Codex writes a token-bearing file into the
            // project dir. GH#168: Claude's MCP config now lives in
            // `~/.claude.json` (home, not project-tracked) — the only file it
            // writes into the project dir is `.claude/settings.json` (hooks),
            // which carries no token. Codex only writes `~/.codex/config.toml`.
            Self::Claude | Self::Codex => &[],
            Self::Cursor => &["cursor.mcp.json"],
            Self::Gemini => &["gemini.mcp.json"],
            Self::Omp => &[".omp/mcp.json"],
            Self::Antigravity => &["agy.mcp.json"],
            Self::OpenCode => &["opencode.json"],
            Self::FactoryDroid => &["factory.mcp.json"],
            Self::Cline => &["cline.mcp.json"],
            Self::Windsurf => &["windsurf.mcp.json"],
            Self::GithubCopilot => &[".vscode/mcp.json"],
        }
    }
}

impl fmt::Display for AgentPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Parse a comma-separated list of agent names into platforms.
pub fn parse_agent_list(input: &str) -> Result<Vec<AgentPlatform>, SetupError> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let platform = AgentPlatform::from_slug(&trimmed.to_ascii_lowercase())
            .ok_or_else(|| SetupError::UnknownPlatform(trimmed.to_string()))?;
        if !out.contains(&platform) {
            out.push(platform);
        }
    }
    Ok(out)
}

/// OMP configuration roots resolved with the same profile and directory
/// precedence as the OMP v18 runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmpConfigPaths {
    /// Root selected by `PI_CONFIG_DIR` (normally `~/.omp`).
    pub config_root: PathBuf,
    /// Active profile's user-level `mcp.json` path.
    pub user_mcp_config: PathBuf,
}

/// Resolve the active OMP user config path from the live process environment.
///
/// OMP gives `OMP_PROFILE` precedence over the legacy `PI_PROFILE`, ignores
/// `PI_CODING_AGENT_DIR` for named profiles, and roots `PI_CONFIG_DIR` beneath
/// the user's home directory. Invalid explicit profile names are rejected,
/// matching OMP's command-line boot behavior; only empty or `default` selects
/// the default profile.
///
/// # Errors
///
/// Returns an error when no absolute home directory is available, or when the
/// explicitly selected `OMP_PROFILE` or legacy `PI_PROFILE` does not satisfy
/// OMP's profile-name contract.
pub fn omp_config_paths_from_env() -> Result<Option<OmpConfigPaths>, SetupError> {
    let home = require_absolute_omp_home_dir(home_dir_for_omp_setup())?;
    let cwd = std::env::current_dir()?;
    let omp_profile = utf8_env_value_for_setup("OMP_PROFILE")?;
    let pi_profile = utf8_env_value_for_setup("PI_PROFILE")?;
    let config_dir = utf8_env_value_for_setup("PI_CONFIG_DIR")?;
    let agent_dir = utf8_env_value_for_setup("PI_CODING_AGENT_DIR")?;

    resolve_omp_config_paths(
        &home,
        &cwd,
        omp_profile.as_deref(),
        pi_profile.as_deref(),
        config_dir.as_deref(),
        agent_dir.as_deref(),
    )
    .map(Some)
}

fn require_absolute_omp_home_dir(home: Option<PathBuf>) -> Result<PathBuf, SetupError> {
    match home {
        Some(home) if omp_path_is_absolute_and_traversal_free(&home) => Ok(home),
        _ => Err(SetupError::Other(
            "cannot resolve the active OMP user config without an absolute, traversal-free home directory"
                .to_string(),
        )),
    }
}

fn omp_path_is_absolute_and_traversal_free(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

fn looks_like_windows_path_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

fn validate_omp_relative_config_dir(path: &str) -> Result<&Path, SetupError> {
    if looks_like_windows_path_prefix(path) {
        return Err(SetupError::Other(
            "PI_CONFIG_DIR must be a traversal-free directory rooted beneath the OMP home directory"
                .to_string(),
        ));
    }
    let path = Path::new(path.trim_start_matches(std::path::is_separator));
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(SetupError::Other(
            "PI_CONFIG_DIR must be a traversal-free directory rooted beneath the OMP home directory"
                .to_string(),
        ));
    }
    Ok(path)
}

fn resolve_omp_agent_dir_override(cwd: &Path, path: &str) -> Result<PathBuf, SetupError> {
    let path = PathBuf::from(path);
    if looks_like_windows_path_prefix(path.to_string_lossy().as_ref()) && !path.is_absolute() {
        return Err(SetupError::Other(
            "PI_CODING_AGENT_DIR must resolve to an absolute, traversal-free directory".to_string(),
        ));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(SetupError::Other(
            "PI_CODING_AGENT_DIR must resolve to an absolute, traversal-free directory".to_string(),
        ));
    }
    let resolved = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    if !omp_path_is_absolute_and_traversal_free(&resolved) {
        return Err(SetupError::Other(
            "PI_CODING_AGENT_DIR must resolve to an absolute, traversal-free directory".to_string(),
        ));
    }
    Ok(resolved)
}

/// Resolve OMP's ordered `PI_CONFIG_FILES` settings overlays from the live
/// environment.
///
/// OMP resolves relative entries against its launch working directory and
/// expands a leading `~` against the active user's home directory. Empty path
/// list entries are ignored, matching OMP's JavaScript loader.
pub fn omp_settings_overlay_paths_from_env(project_dir: &Path) -> Result<Vec<PathBuf>, SetupError> {
    resolve_omp_settings_overlay_paths(
        std::env::var_os("PI_CONFIG_FILES").as_deref(),
        project_dir,
        dirs::home_dir().as_deref(),
    )
}

fn resolve_omp_settings_overlay_paths(
    raw: Option<&OsStr>,
    project_dir: &Path,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, SetupError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    std::env::split_paths(raw)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| {
            let expanded = if path == Path::new("~") {
                home.map(Path::to_path_buf).ok_or_else(|| {
                    SetupError::Other(
                        "PI_CONFIG_FILES contains '~', but the OMP home directory is unavailable"
                            .to_string(),
                    )
                })?
            } else if let Ok(rest) = path.strip_prefix("~") {
                home.map(|home| home.join(rest)).ok_or_else(|| {
                    SetupError::Other(
                        "PI_CONFIG_FILES contains a '~/' path, but the OMP home directory is unavailable"
                            .to_string(),
                    )
                })?
            } else {
                path
            };
            Ok(if expanded.is_absolute() {
                expanded
            } else {
                project_dir.join(expanded)
            })
        })
        .collect()
}

fn resolve_omp_config_paths(
    home: &Path,
    cwd: &Path,
    omp_profile: Option<&str>,
    pi_profile: Option<&str>,
    config_dir: Option<&str>,
    agent_dir_override: Option<&str>,
) -> Result<OmpConfigPaths, SetupError> {
    let home = require_absolute_omp_home_dir(Some(home.to_path_buf()))?;
    if !omp_path_is_absolute_and_traversal_free(cwd) {
        return Err(SetupError::Other(
            "cannot resolve OMP configuration without an absolute, traversal-free working directory"
                .to_string(),
        ));
    }
    let explicit_profile = omp_profile.or(pi_profile);
    let profile = match explicit_profile {
        None => None,
        Some(profile) => {
            let profile = profile.trim();
            if profile.is_empty() || profile == "default" {
                None
            } else {
                Some(normalize_omp_profile_name(profile).ok_or_else(|| {
                    SetupError::Other(format!(
                        "invalid OMP profile {profile:?}; expected lowercase [a-z0-9][a-z0-9._-]{{0,63}}, not '.'/'..', a trailing dot, or a Windows reserved device name"
                    ))
                })?)
            }
        }
    };
    let config_dir = config_dir
        .filter(|value| !value.is_empty())
        .unwrap_or(".omp");
    let config_dir = validate_omp_relative_config_dir(config_dir)?;
    let config_root = home.join(config_dir);

    let agent_dir = profile.map_or_else(
        || {
            agent_dir_override
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || Ok(config_root.join("agent")),
                    |override_path| resolve_omp_agent_dir_override(cwd, override_path),
                )
        },
        |profile| Ok(config_root.join("profiles").join(profile).join("agent")),
    )?;

    let user_mcp_config = agent_dir.join("mcp.json");
    if !omp_path_is_absolute_and_traversal_free(&config_root)
        || !omp_path_is_absolute_and_traversal_free(&user_mcp_config)
    {
        return Err(SetupError::Other(
            "OMP configuration paths must resolve to absolute, traversal-free authorities"
                .to_string(),
        ));
    }

    Ok(OmpConfigPaths {
        config_root,
        user_mcp_config,
    })
}

pub(crate) fn normalize_omp_profile_name(profile: &str) -> Option<&str> {
    let profile = profile.trim();
    if profile.is_empty()
        || profile == "default"
        || profile == "."
        || profile == ".."
        || profile.ends_with('.')
        || profile.len() > 64
        || !profile
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        || !profile.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
    {
        return None;
    }

    let base = profile.split('.').next().unwrap_or(profile);
    let upper = base.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit());
    (!reserved).then_some(profile)
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// A single config file write operation (the unit of work).
pub struct ConfigAction {
    pub platform: AgentPlatform,
    pub file_path: PathBuf,
    pub description: String,
    pub content: ConfigContent,
    pub permissions: u32,
    pub backup: bool,
}

/// How to produce the final file content.
pub enum ConfigContent {
    /// Merge an MCP server entry into existing JSON (or create fresh).
    JsonMerge {
        servers_key: &'static str,
        server_name: &'static str,
        server_value: Value,
        /// Reconcile OMP's active-user-only disable/force-enable lists.
        /// This must remain false for project files, where OMP ignores those
        /// top-level keys and setup must preserve their unrelated bytes.
        reconcile_omp_user_runtime_lists: bool,
    },
    /// Merge an MCP server entry into Claude Code's *local* (per-project) scope:
    /// `projects.<project_path>.mcpServers.<server_name>` inside `~/.claude.json`
    /// (GH#168). This mirrors what `claude mcp add` (default/local scope) writes,
    /// and is one of the only locations the Claude Code v2.x runtime actually
    /// reads MCP servers from. `settings.json`/`settings.local.json` are NOT.
    ClaudeLocalScopeMcp {
        project_path: String,
        server_name: &'static str,
        server_value: Value,
    },
    /// Write complete JSON (for new files only).
    JsonFull(Value),
    /// Merge Claude Code hooks into settings.json.
    HooksMerge {
        project_slug: String,
        agent_name: String,
    },
    /// Append a TOML `[section]` with key-value pairs if not already present.
    TomlSection {
        section_header: String,
        key_values: Vec<(String, String)>,
    },
}

/// Parameters driving the setup.
pub struct SetupParams {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub token: String,
    pub project_dir: PathBuf,
    pub home_dir_override: Option<PathBuf>,
    /// Explicit OMP user `mcp.json` path. Production callers populate this
    /// from [`omp_config_paths_from_env`] so named profiles and OMP directory
    /// overrides target the same file the runtime loads.
    pub omp_user_config_path_override: Option<PathBuf>,
    /// Ordered OMP settings overlays selected through `PI_CONFIG_FILES`.
    /// Production callers resolve these relative to `project_dir` with
    /// [`omp_settings_overlay_paths_from_env`].
    pub omp_settings_overlay_paths: Vec<PathBuf>,
    pub agents: Option<Vec<AgentPlatform>>,
    pub dry_run: bool,
    pub skip_user_config: bool,
    pub skip_hooks: bool,
    pub project_slug: String,
    pub agent_name: String,
}

impl Default for SetupParams {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8765,
            path: "/mcp/".to_string(),
            token: String::new(),
            project_dir: PathBuf::from("."),
            home_dir_override: None,
            omp_user_config_path_override: None,
            omp_settings_overlay_paths: Vec::new(),
            agents: None,
            dry_run: false,
            skip_user_config: false,
            skip_hooks: false,
            project_slug: String::new(),
            agent_name: String::new(),
        }
    }
}

impl SetupParams {
    /// Build the full MCP server URL.
    #[must_use]
    pub fn server_url(&self) -> String {
        format!(
            "http://{}:{}{}",
            normalize_client_connect_host(&self.host),
            self.port,
            self.path
        )
    }
}

#[must_use]
fn normalize_client_connect_host(host: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return std::borrow::Cow::Borrowed("127.0.0.1");
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);
    match unbracketed {
        "0.0.0.0" => std::borrow::Cow::Borrowed("127.0.0.1"),
        "::" => std::borrow::Cow::Borrowed("[::1]"),
        _ => {
            if unbracketed.contains(':') && !trimmed.starts_with('[') {
                std::borrow::Cow::Owned(format!("[{unbracketed}]"))
            } else {
                std::borrow::Cow::Borrowed(trimmed)
            }
        }
    }
}

/// Result of running setup for one agent.
#[derive(Debug, Serialize)]
pub struct SetupResult {
    pub platform: String,
    pub actions: Vec<ActionResult>,
}

/// Result of a single file write.
#[derive(Debug, Serialize)]
pub struct ActionResult {
    pub file_path: String,
    pub description: String,
    pub outcome: ActionOutcome,
}

/// Outcome of a config write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Created,
    Updated,
    Unchanged,
    Skipped,
    BackedUp(String),
    Failed(String),
}

impl fmt::Display for ActionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Updated => write!(f, "updated"),
            Self::Unchanged => write!(f, "unchanged"),
            Self::Skipped => write!(f, "skipped (dry-run)"),
            Self::BackedUp(p) => write!(f, "backed up to {p}"),
            Self::Failed(e) => write!(f, "FAILED: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Token management
// ---------------------------------------------------------------------------

/// Generate a cryptographically random 64-char hex token (256-bit entropy).
pub fn generate_token() -> Result<String, SetupError> {
    let mut bytes = [0u8; 32];
    fill_random_bytes(&mut bytes)?;
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// Generate a cryptographically random URL-safe registration token (256-bit entropy).
///
/// Returns a 43-character base64url-encoded string (no padding) suitable for
/// embedding in JSON responses and passing as `sender_token` parameters.
pub fn generate_registration_token() -> Result<String, SetupError> {
    let mut bytes = [0u8; 32];
    fill_random_bytes(&mut bytes)?;
    Ok(base64url_encode_nopad(&bytes))
}

fn fill_random_bytes(bytes: &mut [u8]) -> Result<(), SetupError> {
    #[cfg(test)]
    if TEST_RANDOM_FAILURE.with(std::cell::Cell::get) {
        return Err(SetupError::Other(
            "CSPRNG failure: test override requested random failure".into(),
        ));
    }

    getrandom::fill(bytes).map_err(|error| {
        SetupError::Other(format!(
            "CSPRNG failure: cannot generate secure token: {error}"
        ))
    })
}

/// Base64url encode without padding (RFC 4648 Section 5).
fn base64url_encode_nopad(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((input.len() * 4).div_ceil(3));
    let chunks = input.chunks(3);
    for chunk in chunks {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Constant-time comparison of two byte slices.
///
/// Returns `true` if both slices are equal in length and content.
/// Always compares all bytes to prevent timing side-channels.
/// Equivalent to Python's `hmac.compare_digest`.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // XOR-fold: accumulate differences without short-circuiting.
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time comparison of two string slices (convenience wrapper).
#[must_use]
pub fn constant_time_str_eq(a: &str, b: &str) -> bool {
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

#[cfg(test)]
thread_local! {
    static TEST_ENV_OVERRIDES: std::cell::RefCell<std::collections::HashMap<String, Option<String>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static TEST_RANDOM_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
const TEST_OMP_HOME_DIR_OVERRIDE_KEY: &str = "__MCP_AGENT_MAIL_TEST_OMP_HOME_DIR";

fn home_dir_for_omp_setup() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(value) = TEST_ENV_OVERRIDES
            .with(|cell| cell.borrow().get(TEST_OMP_HOME_DIR_OVERRIDE_KEY).cloned())
        {
            return value.map(PathBuf::from);
        }
    }
    dirs::home_dir()
}

fn env_value_for_setup(key: &str) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(value) = TEST_ENV_OVERRIDES.with(|cell| cell.borrow().get(key).cloned()) {
            return value;
        }
    }
    std::env::var(key).ok()
}

fn os_env_value_for_setup(key: &str) -> Option<OsString> {
    #[cfg(test)]
    {
        if let Some(value) = TEST_ENV_OVERRIDES.with(|cell| cell.borrow().get(key).cloned()) {
            return value.map(OsString::from);
        }
    }
    std::env::var_os(key)
}

fn utf8_env_value_for_setup(key: &str) -> Result<Option<String>, SetupError> {
    #[cfg(test)]
    {
        if let Some(value) = TEST_ENV_OVERRIDES.with(|cell| cell.borrow().get(key).cloned()) {
            return Ok(value);
        }
    }
    std::env::var_os(key).map_or(Ok(None), |value| {
        value.into_string().map(Some).map_err(|_| {
            SetupError::Other(format!(
                "{key} must contain valid UTF-8; refusing to guess an OMP credential authority"
            ))
        })
    })
}

/// Resolve the bearer token from multiple sources in priority order:
/// explicit flag > config.env file > `HTTP_BEARER_TOKEN` env var > generate new.
pub fn resolve_token(explicit: Option<&str>, env_file: &Path) -> Result<String, SetupError> {
    if let Some(t) = explicit
        && !t.is_empty()
    {
        return Ok(t.to_string());
    }
    if let Some(t) = read_env_file_token(env_file) {
        return Ok(t);
    }
    if let Some(t) = env_value_for_setup("HTTP_BEARER_TOKEN")
        && !t.is_empty()
    {
        return Ok(t);
    }
    generate_token()
}

/// Resolve an existing bearer token without generating or writing a replacement.
#[must_use]
pub fn resolve_existing_token(explicit: Option<&str>, env_file: &Path) -> Option<String> {
    if let Some(token) = explicit
        && !token.is_empty()
    {
        return Some(token.to_string());
    }
    read_env_file_token(env_file)
        .or_else(|| env_value_for_setup("HTTP_BEARER_TOKEN").filter(|token| !token.is_empty()))
}

/// Read `HTTP_BEARER_TOKEN=...` from a .env file.
fn read_env_file_token(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("HTTP_BEARER_TOKEN=") {
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Save the bearer token to a .env file (create or update).
pub fn save_token_to_env_file(env_path: &Path, token: &str) -> Result<(), SetupError> {
    if token.contains('\n') || token.contains('\r') {
        return Err(SetupError::Other("Token must not contain newlines".into()));
    }
    ensure_setup_parent_dir(env_path, "token env file")?;
    validate_setup_file_target(env_path, "token env file")?;

    // Keep token reads under the same no-follow, regular-file-only contract as
    // setup config reads. An `exists()` + `read_to_string()` pair leaves a
    // symlink/FIFO substitution window between validation and the read.
    let existing_content = read_setup_file(env_path, "token env file")?.map(|(content, _)| content);

    let content = existing_content.as_deref().map_or_else(
        || format!("HTTP_BEARER_TOKEN={token}\n"),
        |existing| {
            let mut found = false;
            let updated: Vec<String> = existing
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("HTTP_BEARER_TOKEN=") {
                        found = true;
                        format!("HTTP_BEARER_TOKEN={token}")
                    } else {
                        line.to_string()
                    }
                })
                .collect();
            if found {
                updated.join("\n") + "\n"
            } else {
                let sep = if existing.ends_with('\n') { "" } else { "\n" };
                format!("{existing}{sep}HTTP_BEARER_TOKEN={token}\n")
            }
        },
    );

    ensure_secret_config_not_git_tracked(env_path)?;
    // Always replace the destination atomically, even when its bytes already
    // match.  Content equality says nothing about the credential file's mode
    // or link topology: an idempotent setup run must still repair a
    // world-readable file and detach an attacker-controlled hard link.
    write_setup_file_atomic(env_path, content.as_bytes(), 0o600, "token env file")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON merge
// ---------------------------------------------------------------------------

/// Merge an MCP server entry into existing JSON content.
/// Preserves all existing keys and other MCP servers.
pub fn merge_mcp_server(
    existing: Option<&str>,
    servers_key: &str,
    server_name: &str,
    server_value: Value,
) -> Result<String, SetupError> {
    let mut doc: Value = match existing {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s)?,
        _ => json!({}),
    };

    let obj = doc.as_object_mut().ok_or(SetupError::NotJsonObject)?;
    let servers = obj.entry(servers_key).or_insert_with(|| json!({}));
    let servers_obj = servers.as_object_mut().ok_or(SetupError::NotJsonObject)?;

    if matches!(server_name, "mcp-agent-mail" | "mcp_agent_mail") {
        for alias in ["mcp-agent-mail", "mcp_agent_mail"] {
            if alias != server_name {
                servers_obj.remove(alias);
            }
        }
    }
    servers_obj.insert(server_name.to_string(), server_value);

    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

/// Merge an OMP-native MCP server entry and make the requested server
/// reachable again.
///
/// OMP has two independent disablement surfaces: `enabled: false` on the
/// entry and the active profile's top-level `disabledServers` denylist. A
/// setup run that refreshes the URL/token but leaves either surface in place
/// reports success while OMP still suppresses Agent Mail. Keep the generic
/// JSON merge policy unchanged for other clients and reconcile OMP's native
/// enablement contract only for OMP actions.
fn merge_omp_mcp_server(
    existing: Option<&str>,
    server_name: &str,
    mut server_value: Value,
    reconcile_user_runtime_lists: bool,
) -> Result<String, SetupError> {
    let desired_entry = server_value
        .as_object_mut()
        .ok_or(SetupError::NotJsonObject)?;
    desired_entry.insert("enabled".to_string(), Value::Bool(true));

    let mut doc = match existing {
        Some(content) if !content.trim().is_empty() => serde_json::from_str(content)?,
        _ => json!({}),
    };
    let obj = doc.as_object_mut().ok_or(SetupError::NotJsonObject)?;

    let aliases: &[&str] = if matches!(server_name, "mcp-agent-mail" | "mcp_agent_mail") {
        &OMP_SERVER_ALIASES
    } else {
        &[server_name]
    };
    let native_servers = match obj.get("mcpServers") {
        Some(value) => Some(value.as_object().ok_or(SetupError::NotJsonObject)?),
        None => None,
    };
    let existing_entry = native_servers
        .and_then(|servers| {
            aliases
                .iter()
                .find_map(|name| servers.get(*name).and_then(Value::as_object))
        })
        .or_else(|| {
            ["servers", "mcp", "mcp_servers"]
                .iter()
                .filter_map(|key| obj.get(*key).and_then(Value::as_object))
                .find_map(|servers| {
                    aliases
                        .iter()
                        .find_map(|name| servers.get(*name).and_then(Value::as_object))
                })
        })
        .cloned()
        .unwrap_or_default();

    // Preserve OMP-native neutral tuning fields and unrelated headers, while
    // removing transport/auth fields that are incompatible with the desired
    // HTTP bearer entry. In particular, an explicit OMP OAuth credential can
    // replace the configured Authorization header at runtime, so keeping
    // stale `auth`/`oauth` metadata would make setup and status false-green.
    let mut merged_entry = existing_entry;
    for key in [
        "command",
        "args",
        "cwd",
        "environment",
        "env",
        "transport",
        "httpUrl",
        "http_headers",
        "bearer_token_env_var",
        "auth",
        "oauth",
    ] {
        merged_entry.remove(key);
    }
    let mut headers = merged_entry
        .remove("headers")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
    if let Some(desired_headers) = desired_entry.get("headers").and_then(Value::as_object) {
        headers.extend(desired_headers.clone());
    }
    for (key, value) in desired_entry {
        if key != "headers" {
            merged_entry.insert(key.clone(), value.clone());
        }
    }
    if !headers.is_empty() {
        merged_entry.insert("headers".to_string(), Value::Object(headers));
    }

    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(SetupError::NotJsonObject)?;
    for alias in aliases {
        servers.remove(*alias);
    }
    servers.insert(server_name.to_string(), Value::Object(merged_entry));

    // OMP-native configs have used only `mcpServers`, but early Agent Mail
    // writers emitted its entry under generic legacy containers. Leaving one
    // behind makes status permanently report a duplicate and can let a stale
    // higher-priority definition shadow the repaired HTTP entry.
    for legacy_key in ["servers", "mcp", "mcp_servers"] {
        if let Some(legacy_servers) = obj.get_mut(legacy_key).and_then(Value::as_object_mut) {
            for alias in aliases {
                legacy_servers.remove(*alias);
            }
        }
    }

    if reconcile_user_runtime_lists {
        // These exact-name lists are loaded only from the active user file.
        // The canonical entry now carries `enabled: true`, so stale Agent Mail
        // names are unnecessary in either list. In particular, retaining a
        // historical alias in enabledServers could force-enable a distinct
        // imported entry and create a second live connection.
        for key in ["disabledServers", "enabledServers"] {
            let Some(values) = obj.get_mut(key) else {
                continue;
            };
            let values = values
                .as_array_mut()
                .ok_or_else(|| SetupError::Other(format!("OMP {key} must be a JSON array")))?;
            if values.iter().any(|value| !value.is_string()) {
                return Err(SetupError::Other(format!(
                    "OMP {key} entries must be strings"
                )));
            }
            values.retain(|value| !value.as_str().is_some_and(|name| aliases.contains(&name)));
        }
    }

    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

/// Merge an MCP server entry into Claude Code's local (per-project) scope inside
/// `~/.claude.json`: `projects.<project_path>.mcpServers.<server_name>` (GH#168).
///
/// All unrelated top-level keys (`numStartups`, other `projects`, the top-level
/// user-scope `mcpServers`, …) are preserved. Idempotent: re-running replaces the
/// entry in place and de-dupes the hyphen/underscore alias.
pub fn merge_claude_local_scope_mcp(
    existing: Option<&str>,
    project_path: &str,
    server_name: &str,
    server_value: Value,
) -> Result<String, SetupError> {
    let mut doc: Value = match existing {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s)?,
        _ => json!({}),
    };

    let obj = doc.as_object_mut().ok_or(SetupError::NotJsonObject)?;
    let projects = obj.entry("projects").or_insert_with(|| json!({}));
    let projects_obj = projects.as_object_mut().ok_or(SetupError::NotJsonObject)?;
    let repo = projects_obj
        .entry(project_path.to_string())
        .or_insert_with(|| json!({}));
    let repo_obj = repo.as_object_mut().ok_or(SetupError::NotJsonObject)?;
    let servers = repo_obj.entry("mcpServers").or_insert_with(|| json!({}));
    let servers_obj = servers.as_object_mut().ok_or(SetupError::NotJsonObject)?;

    if matches!(server_name, "mcp-agent-mail" | "mcp_agent_mail") {
        for alias in ["mcp-agent-mail", "mcp_agent_mail"] {
            if alias != server_name {
                servers_obj.remove(alias);
            }
        }
    }
    servers_obj.insert(server_name.to_string(), server_value);

    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

// ---------------------------------------------------------------------------
// Claude Code hooks merge
// ---------------------------------------------------------------------------

/// Markers that identify a hook entry as ours.
const HOOK_MARKERS: &[&str] = &[
    "mcp-agent-mail",
    "am file_reservations",
    "am acks pending",
    "am mail inbox",
];

/// Check if a hook entry is ours (contains any of our markers).
fn hook_is_ours(entry: &Value) -> bool {
    let s = entry.to_string();
    HOOK_MARKERS.iter().any(|m| s.contains(m))
}

/// Build the `SessionStart` hook entries.
fn build_session_start_hooks(project_slug: &str, agent_name: &str) -> Vec<Value> {
    vec![json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": format!("am file_reservations active {project_slug}")
            },
            {
                "type": "command",
                "command": format!("am acks pending {project_slug} {agent_name} --limit 20")
            }
        ]
    })]
}

/// Build the `PreToolUse` hook entries.
fn build_pre_tool_use_hooks(project_slug: &str) -> Vec<Value> {
    vec![json!({
        "matcher": "Edit",
        "hooks": [
            {
                "type": "command",
                "command": format!("am file_reservations soon {project_slug} --minutes 10")
            }
        ]
    })]
}

/// Build the `PostToolUse` hook entries.
///
/// No secrets are embedded — the `am` CLI reads the token from `.env` or
/// `HTTP_BEARER_TOKEN` env var at runtime.
fn build_post_tool_use_hooks(project_slug: &str, agent_name: &str) -> Vec<Value> {
    vec![
        json!({
            "matcher": "Bash",
            "hooks": [
                {
                    "type": "command",
                    "command": format!(
                        "am mail inbox --project {project_slug} --agent {agent_name} --limit 5 2>/dev/null || true"
                    )
                }
            ]
        }),
        json!({
            "matcher": "mcp__mcp-agent-mail__send_message",
            "hooks": [
                {
                    "type": "command",
                    "command": format!("am acks pending {project_slug} {agent_name} --limit 10")
                }
            ]
        }),
        json!({
            "matcher": "mcp__mcp-agent-mail__file_reservation_paths",
            "hooks": [
                {
                    "type": "command",
                    "command": format!("am file_reservations list {project_slug}")
                }
            ]
        }),
    ]
}

fn merge_hook_array(hooks: &mut Map<String, Value>, key: &str, new_entries: Vec<Value>) {
    let arr = hooks.entry(key).or_insert_with(|| json!([]));
    if let Some(arr) = arr.as_array_mut() {
        arr.retain(|entry| !hook_is_ours(entry));
        arr.extend(new_entries);
    }
}

/// Merge our hooks into an existing Claude Code settings.json.
/// Preserves all other settings and user hooks.
///
/// No secrets are embedded in the generated hooks — the `am` CLI reads
/// the bearer token from `.env` or `HTTP_BEARER_TOKEN` at runtime.
pub fn merge_claude_hooks(
    existing: Option<&str>,
    project_slug: &str,
    agent_name: &str,
) -> Result<String, SetupError> {
    let mut doc: Value = match existing {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s)?,
        _ => json!({}),
    };

    let obj = doc.as_object_mut().ok_or(SetupError::NotJsonObject)?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = hooks.as_object_mut().ok_or(SetupError::NotJsonObject)?;

    merge_hook_array(
        hooks_obj,
        "SessionStart",
        build_session_start_hooks(project_slug, agent_name),
    );
    merge_hook_array(
        hooks_obj,
        "PreToolUse",
        build_pre_tool_use_hooks(project_slug),
    );
    merge_hook_array(
        hooks_obj,
        "PostToolUse",
        build_post_tool_use_hooks(project_slug, agent_name),
    );

    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

// ---------------------------------------------------------------------------
// .gitignore management
// ---------------------------------------------------------------------------

/// Ensure the given entries are present in the .gitignore file.
/// Does not duplicate existing entries.
pub fn ensure_gitignore_entries(
    gitignore_path: &Path,
    entries: &[&str],
) -> Result<bool, SetupError> {
    ensure_setup_parent_dir(gitignore_path, "gitignore file")?;
    validate_setup_file_target(gitignore_path, "gitignore file")?;
    let existing = match setup_read_file_options().open(gitignore_path) {
        Ok(mut file) => {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            content
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let existing_lines: Vec<&str> = existing.lines().collect();

    let mut new_lines = Vec::new();
    for entry in entries {
        if !existing_lines.iter().any(|l| l.trim() == *entry) {
            new_lines.push(*entry);
        }
    }

    if new_lines.is_empty() {
        return Ok(false);
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for line in &new_lines {
        content.push_str(line);
        content.push('\n');
    }

    write_setup_file_atomic(gitignore_path, content.as_bytes(), 0o644, "gitignore file")?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// TOML section merge
// ---------------------------------------------------------------------------

/// Merge or append a TOML section, replacing keys in the target section.
///
/// Codex's canonical spelling is `mcp_agent_mail`; rewrite the quoted hyphen
/// alias when setup encounters it so clients do not retain two server names.
fn merge_toml_section(
    existing: Option<&str>,
    section_header: &str,
    key_values: &[(String, String)],
) -> String {
    use std::collections::HashSet;

    let mut section_lines = Vec::with_capacity(key_values.len() + 1);
    section_lines.push(section_header.to_string());
    section_lines.extend(key_values.iter().map(|(k, v)| format!("{k} = {v}")));

    match existing {
        Some(text) if !text.trim().is_empty() => {
            let target_keys: HashSet<&str> = key_values.iter().map(|(k, _)| k.as_str()).collect();
            let mut merged = Vec::new();
            let mut in_target_section = false;
            let mut saw_target_section = false;
            let mut preserved_target_lines = Vec::new();
            let mut preserved_target_keys = HashSet::new();

            for raw_line in text.lines() {
                if let Some(section) = parse_toml_section_header(raw_line) {
                    in_target_section = toml_section_matches_target(section, section_header);
                    saw_target_section |= in_target_section;
                    if !in_target_section {
                        merged.push(raw_line.to_string());
                    }
                    continue;
                }

                if !in_target_section {
                    merged.push(raw_line.to_string());
                    continue;
                }

                // Coalesce every matching spelling of the target table into
                // one canonical section. Keeping the aliases in place would
                // emit duplicate TOML table headers when both spellings were
                // present. Preserve non-managed settings at most once so a
                // pre-existing alias pair remains parseable after setup.
                let Some((lhs, _)) = raw_line.split_once('=') else {
                    preserved_target_lines.push(raw_line.to_string());
                    continue;
                };
                let key = lhs.trim();
                // The desired HTTP section owns every transport/auth key, not
                // only the keys present in this particular invocation. If the
                // server switches from stdio to HTTP, or from bearer auth to
                // no-auth, preserving an omitted managed key leaves Codex on a
                // conflicting transport or stale credential and makes setup's
                // status/self-heal loop unable to converge.
                let setup_managed_key = target_keys.contains(key)
                    || matches!(
                        key,
                        "url"
                            | "httpUrl"
                            | "startup_timeout_sec"
                            | "http_headers"
                            | "env_http_headers"
                            | "bearer_token_env_var"
                            | "command"
                            | "args"
                            | "cwd"
                            | "env"
                            | "environment"
                            | "transport"
                    );
                if !setup_managed_key && preserved_target_keys.insert(key.to_string()) {
                    preserved_target_lines.push(raw_line.to_string());
                }
            }

            if !merged.is_empty() && !merged.last().is_some_and(String::is_empty) {
                merged.push(String::new());
            }
            if saw_target_section {
                merged.push(section_header.to_string());
                merged.extend(preserved_target_lines);
                merged.extend(key_values.iter().map(|(k, v)| format!("{k} = {v}")));
            } else {
                merged.extend(section_lines);
            }

            let mut out = merged.join("\n");
            if text.ends_with('\n') || !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        _ => {
            // No existing file — create fresh.
            let mut section = section_lines.join("\n");
            section.push('\n');
            section
        }
    }
}

fn toml_section_matches_target(section: &str, section_header: &str) -> bool {
    let target = section_header.trim_matches(['[', ']']);
    section == target
        || (target == "mcp_servers.mcp_agent_mail" && section == "mcp_servers.\"mcp-agent-mail\"")
}

fn strip_toml_inline_comment(line: &str) -> &str {
    let mut in_quote = None;
    let mut escape = false;

    for (idx, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match in_quote {
            Some('"') => {
                if ch == '\\' {
                    escape = true;
                } else if ch == '"' {
                    in_quote = None;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    in_quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => in_quote = Some(ch),
                '#' => return line[..idx].trim_end(),
                _ => {}
            },
            Some(_) => {}
        }
    }

    line.trim_end()
}

fn parse_toml_section_header(line: &str) -> Option<&str> {
    let line = strip_toml_inline_comment(line).trim();
    line.strip_prefix('[')?.strip_suffix(']')
}

// ---------------------------------------------------------------------------
// Per-agent config generation
// ---------------------------------------------------------------------------

/// Build the standard MCP server JSON value for HTTP agents.
///
/// When `token` is empty (e.g. `am serve-http --no-auth`), no `Authorization`
/// header is emitted at all — never write a `Bearer ` header with no/blank
/// credential into a project-tracked config (security issue #148).
fn standard_http_server_value(url: &str, token: &str) -> Value {
    if token.is_empty() {
        json!({
            "type": "http",
            "url": url
        })
    } else {
        json!({
            "type": "http",
            "url": url,
            "headers": {
                "Authorization": format!("Bearer {token}")
            }
        })
    }
}

fn omp_http_server_value(url: &str, token: &str) -> Value {
    let mut value = standard_http_server_value(url, token);
    value
        .as_object_mut()
        .expect("standard HTTP server values are JSON objects")
        .insert("enabled".to_string(), Value::Bool(true));
    value
}

/// Build the `headers` object for an MCP server entry, omitting the
/// `Authorization` header entirely when `token` is empty (issue #148).
fn auth_headers_value(token: &str) -> Value {
    if token.is_empty() {
        json!({})
    } else {
        json!({ "Authorization": format!("Bearer {token}") })
    }
}

const CODEX_STATUS_STARTUP_TIMEOUT_SECS: u64 = 30;

/// Helper: create a simple project-local JSON merge action.
fn project_local_action(
    platform: AgentPlatform,
    pdir: &Path,
    filename: &str,
    servers_key: &'static str,
    server_value: Value,
    description: &str,
) -> ConfigAction {
    ConfigAction {
        platform,
        file_path: pdir.join(filename),
        description: description.into(),
        content: ConfigContent::JsonMerge {
            servers_key,
            server_name: "mcp-agent-mail",
            server_value,
            reconcile_omp_user_runtime_lists: false,
        },
        permissions: 0o600,
        backup: true,
    }
}

/// Return the absolute active OMP user-level MCP config path represented by setup parameters.
///
/// This is also the cross-file authority for project-only status: OMP applies
/// its top-level `disabledServers` list to project MCP entries.
///
/// # Errors
///
/// Returns an error rather than treating a literal `~` or another relative
/// path as a filesystem authority. OMP setup files can contain bearer tokens,
/// so a caller must never redirect an unresolved home into its working tree.
pub fn omp_active_user_config_path(params: &SetupParams) -> Result<PathBuf, SetupError> {
    let path = if let Some(path) = &params.omp_user_config_path_override {
        path.clone()
    } else if let Some(home) = &params.home_dir_override {
        require_absolute_omp_home_dir(Some(home.clone()))?
            .join(".omp")
            .join("agent")
            .join("mcp.json")
    } else {
        omp_config_paths_from_env()?
            .ok_or_else(|| {
                SetupError::Other(
                    "cannot resolve the active OMP user config from the live environment"
                        .to_string(),
                )
            })?
            .user_mcp_config
    };
    if !omp_path_is_absolute_and_traversal_free(&path) {
        return Err(SetupError::Other(
            "OMP active-profile user config must be an absolute, traversal-free path".to_string(),
        ));
    }
    Ok(path)
}

fn omp_active_user_secondary_config_path(params: &SetupParams) -> Result<PathBuf, SetupError> {
    Ok(omp_active_user_config_path(params)?
        .parent()
        .ok_or_else(|| {
            SetupError::Other("OMP active-profile user config has no parent directory".to_string())
        })?
        .join(".mcp.json"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OmpMcpAuthorityFormat {
    Native,
    StandardJson,
    CodexToml,
    OpenCodeJsonc,
    VsCodeJson,
    Standalone,
    Unsupported,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OmpMcpAuthorityProvider {
    Native,
    ClaudeProject,
    ClaudeUser,
    Codex,
    Gemini,
    OpenCode,
    Cursor,
    Windsurf,
    VsCode,
    Standalone,
}

struct OmpMcpAuthoritySource {
    path: PathBuf,
    format: OmpMcpAuthorityFormat,
    provider: OmpMcpAuthorityProvider,
    project_level: bool,
}

fn push_omp_mcp_authority_source(
    sources: &mut Vec<OmpMcpAuthoritySource>,
    path: PathBuf,
    format: OmpMcpAuthorityFormat,
    provider: OmpMcpAuthorityProvider,
    project_level: bool,
) {
    if sources.iter().any(|existing| {
        existing.path == path
            && existing.format == format
            && existing.provider == provider
            && existing.project_level == project_level
    }) {
        return;
    }
    sources.push(OmpMcpAuthoritySource {
        path,
        format,
        provider,
        project_level,
    });
}

fn omp_provider_home(params: &SetupParams) -> Option<PathBuf> {
    require_absolute_omp_home_dir(
        params
            .home_dir_override
            .clone()
            .or_else(home_dir_for_omp_setup),
    )
    .ok()
}

fn omp_claude_user_base(params: &SetupParams, home: &Path) -> Result<(PathBuf, PathBuf), PathBuf> {
    if let Some(raw_override) = os_env_value_for_setup("CLAUDE_CONFIG_DIR") {
        let Some(override_path) = raw_override.to_str() else {
            return Err(PathBuf::from(raw_override));
        };
        let override_path = override_path.trim();
        if !override_path.is_empty() {
            let unresolved = PathBuf::from(override_path);
            let config_dir = resolve_omp_agent_dir_override(&params.project_dir, override_path)
                .map_err(|_| unresolved)?;
            return Ok((config_dir.join(".claude.json"), config_dir));
        }
    }
    Ok((home.join(".claude.json"), home.join(".claude")))
}

fn omp_mcp_authority_sources(params: &SetupParams) -> Vec<OmpMcpAuthoritySource> {
    use OmpMcpAuthorityFormat::{
        CodexToml, Native, OpenCodeJsonc, Standalone, StandardJson, Unsupported, VsCodeJson,
    };
    use OmpMcpAuthorityProvider::{
        ClaudeProject, ClaudeUser, Codex, Cursor, Gemini, Native as NativeProvider, OpenCode,
        Standalone as StandaloneProvider, VsCode, Windsurf,
    };

    let project = &params.project_dir;
    let mut sources = Vec::new();

    // Native provider, priority 100. Every file contributes; first key wins.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".omp/mcp.json"),
        Native,
        NativeProvider,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".omp/.mcp.json"),
        Native,
        NativeProvider,
        true,
    );
    if let Ok(path) = omp_active_user_config_path(params) {
        push_omp_mcp_authority_source(&mut sources, path, Native, NativeProvider, false);
    }
    if let Ok(path) = omp_active_user_secondary_config_path(params) {
        push_omp_mcp_authority_source(&mut sources, path, Native, NativeProvider, false);
    }

    // Stable imported tool providers, in descending provider priority. OMP
    // plugin/extension MCP providers are intentionally not guessed here: their
    // roots and manifests are dynamic and remain an explicit status gap.
    let provider_home = omp_provider_home(params);

    // Claude, priority 80: first non-empty project alternative, then first
    // non-empty user alternative.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".claude/.mcp.json"),
        StandardJson,
        ClaudeProject,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".claude/mcp.json"),
        StandardJson,
        ClaudeProject,
        true,
    );
    if let Some(home) = &provider_home {
        match omp_claude_user_base(params, home) {
            Ok((claude_json, claude_dir)) => {
                push_omp_mcp_authority_source(
                    &mut sources,
                    claude_json,
                    StandardJson,
                    ClaudeUser,
                    false,
                );
                push_omp_mcp_authority_source(
                    &mut sources,
                    claude_dir.join("mcp.json"),
                    StandardJson,
                    ClaudeUser,
                    false,
                );
            }
            Err(path) => {
                push_omp_mcp_authority_source(&mut sources, path, Unsupported, ClaudeUser, false)
            }
        }
    } else {
        push_omp_mcp_authority_source(
            &mut sources,
            project.join("<unresolved-claude-user-home>"),
            Unsupported,
            ClaudeUser,
            false,
        );
    }

    // Codex priority 70.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".codex/config.toml"),
        CodexToml,
        Codex,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        provider_home.as_ref().map_or_else(
            || project.join("<unresolved-codex-user-home>"),
            |home| home.join(".codex/config.toml"),
        ),
        if provider_home.is_some() {
            CodexToml
        } else {
            Unsupported
        },
        Codex,
        false,
    );

    // Gemini priority 60.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".gemini/settings.json"),
        StandardJson,
        Gemini,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        provider_home.as_ref().map_or_else(
            || project.join("<unresolved-gemini-user-home>"),
            |home| home.join(".gemini/settings.json"),
        ),
        if provider_home.is_some() {
            StandardJson
        } else {
            Unsupported
        },
        Gemini,
        false,
    );

    // OpenCode priority 55. Its provider deep-merges these low-to-high.
    if let Some(home) = &provider_home {
        for path in [
            home.join(".config/opencode/opencode.json"),
            home.join(".config/opencode/opencode.jsonc"),
        ] {
            push_omp_mcp_authority_source(&mut sources, path, OpenCodeJsonc, OpenCode, false);
        }
    } else {
        push_omp_mcp_authority_source(
            &mut sources,
            project.join("<unresolved-opencode-user-home>"),
            Unsupported,
            OpenCode,
            false,
        );
    }
    for path in [
        project.join("opencode.json"),
        project.join("opencode.jsonc"),
        project.join(".opencode/opencode.json"),
        project.join(".opencode/opencode.jsonc"),
    ] {
        push_omp_mcp_authority_source(&mut sources, path, OpenCodeJsonc, OpenCode, true);
    }

    // Cursor and Windsurf both have priority 50; Cursor registers first.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".cursor/mcp.json"),
        StandardJson,
        Cursor,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        provider_home.as_ref().map_or_else(
            || project.join("<unresolved-cursor-user-home>"),
            |home| home.join(".cursor/mcp.json"),
        ),
        if provider_home.is_some() {
            StandardJson
        } else {
            Unsupported
        },
        Cursor,
        false,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".windsurf/mcp_config.json"),
        StandardJson,
        Windsurf,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        provider_home.as_ref().map_or_else(
            || project.join("<unresolved-windsurf-user-home>"),
            |home| home.join(".codeium/windsurf/mcp_config.json"),
        ),
        if provider_home.is_some() {
            StandardJson
        } else {
            Unsupported
        },
        Windsurf,
        false,
    );

    // VS Code priority 20 (project-only), then standalone root priority 5.
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".vscode/mcp.json"),
        VsCodeJson,
        VsCode,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        project.join("mcp.json"),
        Standalone,
        StandaloneProvider,
        true,
    );
    push_omp_mcp_authority_source(
        &mut sources,
        project.join(".mcp.json"),
        Standalone,
        StandaloneProvider,
        true,
    );

    sources
}

/// Return OMP's stable MCP discovery inputs in exact first-wins load order.
///
/// Setup writes only the canonical native `mcp.json` files. Native `.mcp.json`
/// siblings, stable imported-tool files, and project-root fallbacks remain
/// read-only runtime authorities.
#[must_use]
pub fn omp_mcp_authority_paths(params: &SetupParams) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for source in omp_mcp_authority_sources(params) {
        if source.format != OmpMcpAuthorityFormat::Unsupported && !paths.contains(&source.path) {
            paths.push(source.path);
        }
    }
    paths
}

fn omp_active_user_settings_paths(params: &SetupParams) -> Result<(PathBuf, PathBuf), SetupError> {
    let user_config = omp_active_user_config_path(params)?;
    let agent_dir = user_config.parent().ok_or_else(|| {
        SetupError::Other("OMP active-profile user config has no parent directory".to_string())
    })?;
    Ok((agent_dir.join("config.yml"), agent_dir.join("config.yaml")))
}

fn omp_project_settings_paths(params: &SetupParams) -> Vec<PathBuf> {
    omp_project_settings_sources(params)
        .into_iter()
        .map(|source| source.path)
        .collect()
}

/// Return every settings file whose current bytes can decide whether OMP loads
/// project MCP configuration for these setup parameters.
///
/// OMP prefers the active profile's `config.yml` over `config.yaml`; the
/// fallback path is relevant only while the preferred path is absent. Every
/// registered persistent project-settings provider follows in OMP's merge
/// order, then the ordered `PI_CONFIG_FILES` overlays.
#[must_use]
pub fn omp_settings_authority_paths(params: &SetupParams) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok((user_yml, user_yaml)) = omp_active_user_settings_paths(params) {
        paths.push(user_yml.clone());
        if user_yml
            .symlink_metadata()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            paths.push(user_yaml);
        }
    }
    paths.extend(omp_project_settings_paths(params));
    paths.extend(params.omp_settings_overlay_paths.iter().cloned());
    paths
}

impl AgentPlatform {
    /// Generate config file actions for this platform.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn config_actions(self, params: &SetupParams) -> Vec<ConfigAction> {
        // `ConfigAction` and `write_config_atomic` are public because the CLI's
        // repair/fingerprint surfaces consume them directly. Do not hand a
        // caller even the project bearer action until OMP's active-user
        // authority is known: otherwise a caller could bypass `run_setup`'s
        // preflight and write a token while a named/custom profile remains
        // unresolved.
        if self == Self::Omp && omp_active_user_config_path(params).is_err() {
            return Vec::new();
        }
        let url = params.server_url();
        let token = &params.token;
        let pdir = &params.project_dir;
        let home = params
            .home_dir_override
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("~"));

        match self {
            Self::Claude => self.claude_actions(params, &url, token, pdir, &home),
            Self::Cursor => self.cursor_actions(params, &url, token, pdir, &home),
            Self::Cline => vec![project_local_action(
                self,
                pdir,
                "cline.mcp.json",
                "mcpServers",
                standard_http_server_value(&url, token),
                "Cline project-local MCP config",
            )],
            Self::Windsurf => vec![project_local_action(
                self,
                pdir,
                "windsurf.mcp.json",
                "mcpServers",
                standard_http_server_value(&url, token),
                "Windsurf project-local MCP config",
            )],
            Self::Codex => {
                let mut key_values = vec![
                    ("url".into(), format!("\"{url}\"")),
                    (
                        "startup_timeout_sec".into(),
                        CODEX_STATUS_STARTUP_TIMEOUT_SECS.to_string(),
                    ),
                ];
                if !token.is_empty() {
                    key_values.push((
                        "http_headers".into(),
                        format!("{{ Authorization = \"Bearer {token}\" }}"),
                    ));
                }
                vec![ConfigAction {
                    platform: self,
                    file_path: home.join(".codex").join("config.toml"),
                    description: "Codex CLI TOML config (~/.codex/config.toml)".into(),
                    content: ConfigContent::TomlSection {
                        section_header: "[mcp_servers.mcp_agent_mail]".into(),
                        key_values,
                    },
                    permissions: 0o600,
                    backup: true,
                }]
            }
            Self::Gemini => self.gemini_actions(params, &url, token, pdir, &home),
            Self::Omp => self.omp_actions(params, &url, token, pdir),
            Self::Antigravity => self.antigravity_actions(params, &url, token, pdir, &home),
            Self::OpenCode => vec![project_local_action(
                self,
                pdir,
                "opencode.json",
                "mcp",
                json!({
                    "type": "remote",
                    "url": url,
                    "headers": auth_headers_value(token),
                    "enabled": true
                }),
                "OpenCode project-local MCP config",
            )],
            Self::FactoryDroid => self.factory_actions(params, &url, token, pdir, &home),
            Self::GithubCopilot => vec![ConfigAction {
                platform: self,
                file_path: pdir.join(".vscode").join("mcp.json"),
                description: "GitHub Copilot MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "servers",
                    server_name: "mcp-agent-mail",
                    server_value: standard_http_server_value(&url, token),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o600,
                backup: true,
            }],
        }
    }

    fn claude_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
        home: &Path,
    ) -> Vec<ConfigAction> {
        // GH#168: Claude Code v2.x reads MCP servers ONLY from `~/.claude.json`
        // (top-level `mcpServers` = user scope; `projects.<abs>.mcpServers` =
        // local/per-project scope) and project `.mcp.json` — NEVER from
        // `settings.json`/`settings.local.json` (those are hooks/permissions).
        // Writing the old location left every fresh `claude` instance with zero
        // Agent Mail tools. Mirror `claude mcp add`: local scope per-project +
        // user scope top-level, both in `~/.claude.json` (home, not git-tracked,
        // so the bearer token never lands in the project working tree).
        let claude_json = home.join(".claude.json");
        let project_key = pdir.to_string_lossy().into_owned();
        let mut actions = vec![ConfigAction {
            platform: self,
            file_path: claude_json.clone(),
            description:
                "Claude Code project-local MCP config (~/.claude.json local scope; secrets)".into(),
            content: ConfigContent::ClaudeLocalScopeMcp {
                project_path: project_key,
                server_name: "mcp-agent-mail",
                server_value: standard_http_server_value(url, token),
            },
            permissions: 0o600,
            backup: true,
        }];
        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: claude_json,
                description: "Claude Code user-level MCP config (~/.claude.json top-level mcpServers)"
                    .into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: standard_http_server_value(url, token),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o600,
                backup: true,
            });
        }

        if !params.skip_hooks {
            actions.push(ConfigAction {
                platform: self,
                file_path: pdir.join(".claude").join("settings.json"),
                description: "Claude Code hooks (git-tracked)".into(),
                content: ConfigContent::HooksMerge {
                    project_slug: params.project_slug.clone(),
                    agent_name: params.agent_name.clone(),
                },
                permissions: 0o644,
                backup: true,
            });
        }

        actions
    }

    fn cursor_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
        home: &Path,
    ) -> Vec<ConfigAction> {
        let mut actions = vec![project_local_action(
            self,
            pdir,
            "cursor.mcp.json",
            "mcpServers",
            standard_http_server_value(url, token),
            "Cursor project-local MCP config",
        )];
        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: home.join(".cursor").join("mcp.json"),
                description: "Cursor user-level MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: json!({ "type": "http", "url": url }),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o644,
                backup: true,
            });
        }
        actions
    }

    fn gemini_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
        home: &Path,
    ) -> Vec<ConfigAction> {
        let mut actions = vec![project_local_action(
            self,
            pdir,
            "gemini.mcp.json",
            "mcpServers",
            json!({
                "httpUrl": url,
                "headers": auth_headers_value(token)
            }),
            "Gemini CLI project-local MCP config",
        )];
        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: home.join(".gemini").join("settings.json"),
                description: "Gemini CLI user-level MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: json!({ "httpUrl": url }),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o644,
                backup: true,
            });
        }
        actions
    }

    /// OMP-native MCP config actions.
    ///
    /// OMP loads both project `.omp/mcp.json` and the active profile's user
    /// config. The project file is therefore the profile-independent setup
    /// surface; the active-profile user file is also refreshed unless the
    /// caller requests project-local configuration only.
    fn omp_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
    ) -> Vec<ConfigAction> {
        let mut actions = vec![self.omp_project_action(url, token, pdir)];
        if !params.skip_user_config
            && let Ok(file_path) = omp_active_user_config_path(params)
        {
            actions.push(ConfigAction {
                platform: self,
                file_path,
                description: "Oh My Pi (OMP) active-profile MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: omp_http_server_value(url, token),
                    reconcile_omp_user_runtime_lists: true,
                },
                permissions: 0o600,
                backup: true,
            });
        }
        actions
    }

    fn omp_project_action(self, url: &str, token: &str, pdir: &Path) -> ConfigAction {
        project_local_action(
            self,
            pdir,
            ".omp/mcp.json",
            "mcpServers",
            omp_http_server_value(url, token),
            "Oh My Pi (OMP) project-local MCP config",
        )
    }

    /// Antigravity (`agy`) MCP config actions.
    ///
    /// agy is the successor to the retired Gemini CLI and consumes the
    /// gemini-compatible `mcpServers` schema, but from a DIFFERENT file:
    /// the canonical user-level path is `~/.gemini/config/mcp_config.json`
    /// (NOT Gemini's `~/.gemini/settings.json`). This was verified empirically
    /// by stracing the live agy 1.0.7 binary, which opens
    /// `~/.gemini/config/mcp_config.json` at session start and spawns the
    /// configured stdio `command`. The HTTP form uses `httpUrl` + `headers`,
    /// identical to Gemini's MCP entry shape.
    ///
    /// Token safety (issue #148): the user-level `mcp_config.json` carries NO
    /// bearer token; only the project-local `agy.mcp.json` embeds the
    /// `Authorization` header, and that file is force-added to `.gitignore`
    /// via `project_local_secret_files()`.
    fn antigravity_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
        home: &Path,
    ) -> Vec<ConfigAction> {
        let mut actions = vec![project_local_action(
            self,
            pdir,
            "agy.mcp.json",
            "mcpServers",
            json!({
                "httpUrl": url,
                "headers": auth_headers_value(token)
            }),
            "Antigravity (agy) project-local MCP config",
        )];
        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: home.join(".gemini").join("config").join("mcp_config.json"),
                description: "Antigravity (agy) user-level MCP config \
                              (~/.gemini/config/mcp_config.json)"
                    .into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: json!({ "httpUrl": url }),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o644,
                backup: true,
            });
        }
        actions
    }

    fn factory_actions(
        self,
        params: &SetupParams,
        url: &str,
        token: &str,
        pdir: &Path,
        home: &Path,
    ) -> Vec<ConfigAction> {
        let mut actions = vec![project_local_action(
            self,
            pdir,
            "factory.mcp.json",
            "mcpServers",
            json!({
                "url": url,
                "headers": auth_headers_value(token)
            }),
            "Factory Droid project-local MCP config",
        )];
        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: home.join(".factory").join("mcp.json"),
                description: "Factory Droid user-level MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: json!({ "url": url }),
                    reconcile_omp_user_runtime_lists: false,
                },
                permissions: 0o644,
                backup: true,
            });
        }
        actions
    }
}

// ---------------------------------------------------------------------------
// Atomic file writes
// ---------------------------------------------------------------------------

fn invalid_setup_path(label: &str, path: &Path, reason: impl fmt::Display) -> SetupError {
    SetupError::Other(format!("{label} {}: {}", reason, path.display()))
}

fn ensure_no_parent_traversal(path: &Path, label: &str) -> Result<(), SetupError> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid_setup_path(
            label,
            path,
            "must not contain parent traversal",
        ));
    }
    Ok(())
}

fn check_setup_real_directory(
    path: &Path,
    label: &str,
    create_missing: bool,
) -> Result<(), SetupError> {
    ensure_no_parent_traversal(path, label)?;

    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            std::path::Component::RootDir => current.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => unreachable!("parent traversal checked above"),
            std::path::Component::Normal(segment) => {
                current.push(segment);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink()
                            && crate::disk::is_trusted_system_directory_alias(&current)
                        {
                            continue;
                        }
                        if metadata.file_type().is_symlink() {
                            return Err(invalid_setup_path(
                                label,
                                &current,
                                "must not traverse symlinked directories",
                            ));
                        }
                        if !metadata.file_type().is_dir() {
                            return Err(invalid_setup_path(
                                label,
                                &current,
                                "expected directory component",
                            ));
                        }
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound && create_missing =>
                    {
                        std::fs::create_dir(&current)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(invalid_setup_path(
                            label,
                            &current,
                            "directory component does not exist",
                        ));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(())
}

fn ensure_setup_real_directory(path: &Path, label: &str) -> Result<(), SetupError> {
    check_setup_real_directory(path, label, true)
}

fn validate_setup_real_directory(path: &Path, label: &str) -> Result<(), SetupError> {
    check_setup_real_directory(path, label, false)
}

fn ensure_setup_parent_dir(path: &Path, label: &str) -> Result<(), SetupError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    ensure_setup_real_directory(parent, label)
}

fn validate_setup_file_target(path: &Path, label: &str) -> Result<(), SetupError> {
    ensure_no_parent_traversal(path, label)?;
    if path.file_name().is_none_or(std::ffi::OsStr::is_empty) {
        return Err(invalid_setup_path(label, path, "must name a file"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(invalid_setup_path(label, path, "must not be a symlink"));
            }
            if !metadata.file_type().is_file() {
                return Err(invalid_setup_path(label, path, "must be a file path"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn setup_new_file_options(permissions: u32) -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(permissions)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    }
    options
}

fn setup_read_file_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    options
}

fn read_setup_file(path: &Path, label: &str) -> Result<Option<(String, u32)>, SetupError> {
    let mut file = match setup_read_file_options().open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid_setup_path(label, path, "must be a regular file"));
    }

    let mut content = String::new();
    file.read_to_string(&mut content)?;

    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let permissions = 0o777;

    Ok(Some((content, permissions)))
}

fn create_unique_setup_file(
    parent: &Path,
    file_name: &str,
    suffix: &str,
    permissions: u32,
) -> Result<(PathBuf, std::fs::File), SetupError> {
    let pid = std::process::id();
    let now = crate::timestamps::now_micros();
    for attempt in 0..1024 {
        let candidate = parent.join(format!(".{file_name}.{pid}.{now}.{attempt}.{suffix}"));
        match setup_new_file_options(permissions).open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(SetupError::Other(format!(
        "could not create unique temporary setup file next to {}",
        parent.display()
    )))
}

fn write_setup_file_atomic(
    path: &Path,
    content: &[u8],
    permissions: u32,
    label: &str,
) -> Result<(), SetupError> {
    ensure_setup_parent_dir(path, label)?;
    validate_setup_file_target(path, label)?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_setup_path(label, path, "must name a file"))?
        .to_string_lossy();
    let (temp, mut file) = create_unique_setup_file(parent, &file_name, "tmp", permissions)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(permissions))?;
    }

    validate_setup_file_target(path, label)?;
    std::fs::rename(&temp, path)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn write_setup_backup(
    content: &[u8],
    backup_parent: &Path,
    file_name: &str,
    permissions: u32,
) -> Result<(), SetupError> {
    let (backup, mut output) =
        create_unique_setup_file(backup_parent, file_name, "bak", permissions)?;
    validate_setup_file_target(&backup, "config backup target")?;
    output.write_all(content)?;
    output.sync_all()?;
    drop(output);
    #[cfg(unix)]
    std::fs::File::open(backup_parent)?.sync_all()?;
    Ok(())
}

fn escape_gitignore_literal(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '\\' | '!' | '#' | '[' | ']' | '*' | '?' | ' ') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn gitignore_relative_path(path: &Path) -> Result<String, SetupError> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                let segment = segment.to_str().ok_or_else(|| {
                    SetupError::Other(format!(
                        "cannot protect non-UTF-8 secret config path {} with .gitignore",
                        path.display()
                    ))
                })?;
                if segment.contains('\r') || segment.contains('\n') {
                    return Err(SetupError::Other(format!(
                        "secret config path {} must not contain CR or LF",
                        path.display()
                    )));
                }
                segments.push(segment);
            }
            std::path::Component::CurDir => {}
            _ => {
                return Err(SetupError::Other(format!(
                    "secret config path {} is not a normal relative path",
                    path.display()
                )));
            }
        }
    }
    if segments.is_empty() {
        return Err(SetupError::Other(
            "secret config path must name a relative file".to_string(),
        ));
    }
    Ok(segments.join("/"))
}

fn resolve_secret_git_context(
    path: &Path,
    secure_gitignore: bool,
) -> Result<Option<(PathBuf, PathBuf)>, SetupError> {
    const REPOSITORY_SHAPING_GIT_ENV: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ];
    if let Some(variable) = REPOSITORY_SHAPING_GIT_ENV
        .iter()
        .find(|variable| std::env::var_os(variable).is_some())
    {
        return Err(SetupError::Other(format!(
            "cannot verify whether secret config {} is Git-tracked while {variable} changes repository discovery or index authority",
            path.display()
        )));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_setup_path("secret config", path, "must name a file"))?;
    validate_setup_real_directory(parent, "secret config parent")?;
    validate_setup_file_target(path, "secret config")?;
    let _ = read_setup_file(path, "secret config")?;
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        SetupError::Other(format!(
            "cannot resolve secret config parent {}: {error}",
            parent.display()
        ))
    })?;
    let repo_probe = crate::git_cmd::GitCmd::new(&parent)
        .env("LC_ALL", "C")
        .args(["rev-parse", "--show-toplevel"]);
    let repo_probe = if secure_gitignore {
        repo_probe
    } else {
        repo_probe.skip_flock()
    }
    .run()
    .map_err(|error| {
        SetupError::Other(format!(
            "cannot verify whether secret config {} is Git-tracked: {error}",
            path.display()
        ))
    })?;
    if !repo_probe.status.success() {
        let stderr = String::from_utf8_lossy(&repo_probe.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(SetupError::Other(format!(
            "cannot verify whether secret config {} is Git-tracked: git rev-parse failed: {}",
            path.display(),
            stderr.trim()
        )));
    }
    let repo_root_output = String::from_utf8(repo_probe.stdout).map_err(|_| {
        SetupError::Other(format!(
            "cannot verify whether secret config {} is Git-tracked: repository root is not UTF-8",
            path.display()
        ))
    })?;
    let repo_root = std::fs::canonicalize(repo_root_output.trim()).map_err(|error| {
        SetupError::Other(format!(
            "cannot resolve Git worktree for secret config {}: {error}",
            path.display()
        ))
    })?;
    let target = parent.join(file_name);
    let relative = target.strip_prefix(&repo_root).map_err(|_| {
        SetupError::Other(format!(
            "secret config {} is outside the resolved Git worktree {}",
            path.display(),
            repo_root.display()
        ))
    })?;
    Ok(Some((repo_root, relative.to_path_buf())))
}

fn verify_secret_gitignore_protection(
    repo_root: &Path,
    representative_paths: [String; 4],
) -> Result<(), SetupError> {
    for representative in representative_paths {
        // `git check-ignore` does not accept the global `--literal-pathspecs`
        // option. Feed the one pathname through its NUL-delimited stdin mode
        // so Git treats metacharacters as pathname bytes, not pathspec magic.
        let mut pathname = representative.as_bytes().to_vec();
        pathname.push(0);
        let ignored = crate::git_cmd::GitCmd::new(repo_root)
            .env("LC_ALL", "C")
            .args(["check-ignore", "--no-index", "--stdin", "-z"])
            .stdin(pathname)
            .skip_flock()
            .skip_mutex()
            .run()
            .map_err(|error| {
                SetupError::Other(format!(
                    "cannot verify .gitignore protection for secret artifact {representative}: {error}"
                ))
            })?;
        if !ignored.status.success() {
            let stderr = String::from_utf8_lossy(&ignored.stderr);
            return Err(SetupError::Other(format!(
                "refusing secret write because .gitignore does not protect artifact {representative} (git status {}; {})",
                ignored.status,
                stderr.trim()
            )));
        }
    }
    Ok(())
}

fn protect_untracked_secret_config(
    repo_root: &Path,
    relative: &Path,
    secure_gitignore: bool,
) -> Result<(), SetupError> {
    let relative = gitignore_relative_path(relative)?;
    let (directory, basename) = relative.rsplit_once('/').unwrap_or(("", relative.as_str()));
    let escaped_directory = escape_gitignore_literal(directory);
    let escaped_basename = escape_gitignore_literal(basename);
    let escaped_relative = escape_gitignore_literal(&relative);
    let prefix = if escaped_directory.is_empty() {
        "/".to_string()
    } else {
        format!("/{escaped_directory}/")
    };
    let entries = [
        format!("/{escaped_relative}"),
        format!("{prefix}.{escaped_basename}.*.tmp"),
        format!("{prefix}.{escaped_basename}.*.bak"),
        format!("/{escaped_relative}.bak*"),
    ];
    let gitignore = repo_root.join(".gitignore");
    validate_setup_file_target(&gitignore, "gitignore file")?;
    let _ = read_setup_file(&gitignore, "gitignore file")?;
    if !secure_gitignore {
        return Ok(());
    }
    let entry_refs = entries.iter().map(String::as_str).collect::<Vec<_>>();
    ensure_gitignore_entries(&gitignore, &entry_refs)?;
    verify_secret_gitignore_protection(
        repo_root,
        [
            relative.clone(),
            format!("{directory}/.{basename}.probe.tmp")
                .trim_start_matches('/')
                .to_string(),
            format!("{directory}/.{basename}.probe.bak")
                .trim_start_matches('/')
                .to_string(),
            format!("{relative}.bak"),
        ],
    )
}

fn check_secret_config_git_exposure_locked(
    path: &Path,
    repo_root: &Path,
    relative: &Path,
    secure_gitignore: bool,
) -> Result<(), SetupError> {
    let tracked_probe = crate::git_cmd::GitCmd::new(repo_root)
        .env("LC_ALL", "C")
        .args(["--literal-pathspecs", "ls-files", "--error-unmatch", "--"])
        .arg(relative.as_os_str())
        .skip_flock()
        .skip_mutex()
        .run()
        .map_err(|error| {
            SetupError::Other(format!(
                "cannot verify whether secret config {} is Git-tracked: {error}",
                path.display()
            ))
        })?;
    if tracked_probe.status.success() {
        return Err(SetupError::Other(format!(
            "refusing to write a literal bearer token to Git-tracked config {}; remove it from the index or use an untracked config path",
            path.display()
        )));
    }
    if tracked_probe.status.code() == Some(1) {
        return protect_untracked_secret_config(repo_root, relative, secure_gitignore);
    }
    Err(SetupError::Other(format!(
        "cannot verify whether secret config {} is Git-tracked: git ls-files failed",
        path.display()
    )))
}

/// Secure a literal-credential config before writing it in a Git worktree.
///
/// Adding a path to `.gitignore` does not make an already tracked file safe:
/// the next `git diff` or commit would still expose the credential. This check
/// therefore refuses tracked targets, then atomically appends anchored ignore
/// rules for the live file and every adjacent backup/temp name used by setup
/// and doctor. If Git cannot answer reliably, it fails closed.
fn check_secret_config_git_exposure(path: &Path, secure_gitignore: bool) -> Result<(), SetupError> {
    let Some((repo_root, relative)) = resolve_secret_git_context(path, secure_gitignore)? else {
        return Ok(());
    };
    // Hold one cooperative lock across the tracked-file probe, .gitignore
    // read/append, and post-write verification. Locking only the individual
    // Git commands leaves the intervening atomic file rewrite exposed to a
    // lost-update race between concurrent setup/doctor processes.
    let secret_mutex = secure_gitignore.then(|| crate::GitRepoLocks::global().lock_for(&repo_root));
    let _secret_mutex_guard = secret_mutex.as_ref().map(|mutex| {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    let _secret_flock_guard = if secure_gitignore {
        let guard = crate::RepoFlock::acquire(&repo_root).map_err(|error| {
            SetupError::Other(format!(
                "cannot serialize secret-config protection in {}: {error}",
                repo_root.display()
            ))
        })?;
        if !guard.is_real() {
            return Err(SetupError::Other(format!(
                "cannot serialize secret-config protection in {} because its Git lock sentinel is unavailable",
                repo_root.display()
            )));
        }
        Some(guard)
    } else {
        None
    };
    // The outer guard above owns the flock for mutating calls; dry-run calls
    // deliberately create no sentinel. In either mode an inner flock would
    // be redundant (and would deadlock when the outer guard is real).
    check_secret_config_git_exposure_locked(path, &repo_root, &relative, secure_gitignore)
}

/// Read-only preflight for a secret-bearing config write.
pub fn preflight_secret_config_not_git_tracked(path: &Path) -> Result<(), SetupError> {
    check_secret_config_git_exposure(path, false)
}

/// Refuse tracked targets and secure ignore rules for an untracked secret file.
pub fn ensure_secret_config_not_git_tracked(path: &Path) -> Result<(), SetupError> {
    check_secret_config_git_exposure(path, true)
}

/// Execute a single config write action, returning the outcome.
pub fn write_config_atomic(
    action: &ConfigAction,
    contains_literal_secret: bool,
) -> Result<ActionOutcome, SetupError> {
    let parent = action.file_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_setup_parent_dir(&action.file_path, "config file")?;
    validate_setup_file_target(&action.file_path, "config file")?;

    // Read through the same no-follow discipline as the write path. Treating
    // an arbitrary read failure as "missing" would let setup overwrite an
    // unreadable or non-UTF config without a backup.
    let existing_file = read_setup_file(&action.file_path, "config file")?;
    let existing = existing_file.as_ref().map(|(content, _)| content.clone());

    // Never widen an existing config's permissions. Conversely, when setup is
    // adding a secret to a previously broad file, tighten it to the action's
    // requested mode. Backups use the same effective mode so they cannot leak
    // the pre-update contents.
    let effective_permissions = existing_file
        .as_ref()
        .map_or(action.permissions, |(_, mode)| {
            #[cfg(unix)]
            {
                *mode & action.permissions
            }
            #[cfg(not(unix))]
            {
                let _ = mode;
                action.permissions
            }
        });
    #[cfg(unix)]
    let permissions_need_tightening = existing_file
        .as_ref()
        .is_some_and(|(_, mode)| *mode != effective_permissions);
    #[cfg(not(unix))]
    let permissions_need_tightening = false;

    let new_content = match &action.content {
        ConfigContent::JsonMerge {
            servers_key,
            server_name,
            server_value,
            reconcile_omp_user_runtime_lists,
        } => {
            if action.platform == AgentPlatform::Omp {
                debug_assert_eq!(*servers_key, "mcpServers");
                merge_omp_mcp_server(
                    existing.as_deref(),
                    server_name,
                    server_value.clone(),
                    *reconcile_omp_user_runtime_lists,
                )?
            } else {
                merge_mcp_server(
                    existing.as_deref(),
                    servers_key,
                    server_name,
                    server_value.clone(),
                )?
            }
        }
        ConfigContent::ClaudeLocalScopeMcp {
            project_path,
            server_name,
            server_value,
        } => merge_claude_local_scope_mcp(
            existing.as_deref(),
            project_path,
            server_name,
            server_value.clone(),
        )?,
        ConfigContent::JsonFull(val) => serde_json::to_string_pretty(val)? + "\n",
        ConfigContent::HooksMerge {
            project_slug,
            agent_name,
        } => merge_claude_hooks(existing.as_deref(), project_slug, agent_name)?,
        ConfigContent::TomlSection {
            section_header,
            key_values,
        } => merge_toml_section(existing.as_deref(), section_header, key_values),
    };

    let existing_may_contain_secret = existing.as_deref().is_some_and(|content| {
        content.contains("Bearer ") || content.contains("HTTP_BEARER_TOKEN")
    });
    let output_contains_literal_secret = new_content.contains("Bearer ");
    if contains_literal_secret || existing_may_contain_secret || output_contains_literal_secret {
        ensure_secret_config_not_git_tracked(&action.file_path)?;
    }

    // Check if unchanged
    if existing.as_deref() == Some(&new_content) && !permissions_need_tightening {
        return Ok(ActionOutcome::Unchanged);
    }

    let was_existing = existing.is_some();

    // Backup existing file
    if action.backup && was_existing {
        let file_name = action
            .file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        write_setup_backup(
            existing
                .as_deref()
                .expect("an existing setup file has pre-update content")
                .as_bytes(),
            parent,
            &file_name,
            effective_permissions,
        )?;
    }

    write_setup_file_atomic(
        &action.file_path,
        new_content.as_bytes(),
        effective_permissions,
        "config file",
    )?;

    if was_existing {
        Ok(ActionOutcome::Updated)
    } else {
        Ok(ActionOutcome::Created)
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

fn project_secret_gitignore_error(
    params: &SetupParams,
    platforms: &[AgentPlatform],
    project_token_paths: &std::collections::BTreeSet<PathBuf>,
) -> Option<String> {
    if params.dry_run {
        return None;
    }
    let gitignore = params.project_dir.join(".gitignore");
    let mut entries = vec![".env".to_string()];
    for platform in platforms {
        for file in platform.project_local_secret_files() {
            let entry = (*file).to_string();
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    for path in project_token_paths {
        let Ok(relative) = path.strip_prefix(&params.project_dir) else {
            continue;
        };
        let relative = match gitignore_relative_path(relative) {
            Ok(relative) => relative,
            Err(error) => return Some(error.to_string()),
        };
        let entry = format!("/{}", escape_gitignore_literal(&relative));
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    let entry_refs = entries.iter().map(String::as_str).collect::<Vec<_>>();
    ensure_gitignore_entries(&gitignore, &entry_refs)
        .err()
        .map(|error| error.to_string())
}

fn omp_setup_authority_preflight(
    params: &SetupParams,
    platforms: &[AgentPlatform],
) -> Option<SetupResult> {
    if !platforms.contains(&AgentPlatform::Omp) {
        return None;
    }
    let error = omp_active_user_config_path(params).err()?;
    let requested_path = params.omp_user_config_path_override.as_deref().map_or_else(
        || "<unresolved>".to_string(),
        |path| path.display().to_string(),
    );
    Some(SetupResult {
        platform: AgentPlatform::Omp.display_name().to_string(),
        actions: vec![ActionResult {
            file_path: requested_path,
            description: "Oh My Pi (OMP) active-profile MCP authority preflight".to_string(),
            outcome: ActionOutcome::Failed(format!(
                "refusing OMP setup before writing any config bytes: {error}"
            )),
        }],
    })
}

/// Run the full setup flow.
#[must_use]
pub fn run_setup(params: &SetupParams) -> Vec<SetupResult> {
    let platforms = params
        .agents
        .clone()
        .unwrap_or_else(|| AgentPlatform::ALL.to_vec());
    if let Some(failure) = omp_setup_authority_preflight(params, &platforms) {
        return vec![failure];
    }
    let project_token_paths = if params.token.is_empty() {
        std::collections::BTreeSet::new()
    } else {
        platforms
            .iter()
            .flat_map(|platform| platform.config_actions(params))
            .filter(|action| {
                expected_authorization_for_action(action, &params.token).is_some()
                    && action.file_path.strip_prefix(&params.project_dir).is_ok()
            })
            .map(|action| action.file_path)
            .collect::<std::collections::BTreeSet<_>>()
    };

    // Establish project-secret exclusions before writing any token-bearing
    // client config. Reporting success after a symlinked, unreadable, or
    // otherwise unmodifiable `.gitignore` would leave a live bearer token
    // exposed to the next `git add -A`.
    let gitignore_error = project_secret_gitignore_error(params, &platforms, &project_token_paths);

    let mut results = Vec::new();

    for platform in &platforms {
        let actions = platform.config_actions(params);
        let mut action_results = Vec::new();

        for action in &actions {
            let is_token_bearing_project_config = project_token_paths.contains(&action.file_path);
            let contains_literal_secret =
                expected_authorization_for_action(action, &params.token).is_some();
            let outcome = if let Some(error) = gitignore_error.as_deref()
                && is_token_bearing_project_config
            {
                ActionOutcome::Failed(format!(
                    "refusing to write a token-bearing project config because .gitignore could not be secured: {error}"
                ))
            } else if params.dry_run
                && contains_literal_secret
                && action
                    .file_path
                    .parent()
                    .is_some_and(std::path::Path::exists)
            {
                match preflight_secret_config_not_git_tracked(&action.file_path) {
                    Ok(()) => ActionOutcome::Skipped,
                    Err(error) => ActionOutcome::Failed(format!(
                        "dry-run preflight refused a token-bearing config: {error}"
                    )),
                }
            } else if params.dry_run {
                ActionOutcome::Skipped
            } else {
                match write_config_atomic(action, contains_literal_secret) {
                    Ok(o) => o,
                    Err(e) => ActionOutcome::Failed(e.to_string()),
                }
            };

            action_results.push(ActionResult {
                file_path: action.file_path.display().to_string(),
                description: action.description.clone(),
                outcome,
            });
        }

        results.push(SetupResult {
            platform: platform.display_name().to_string(),
            actions: action_results,
        });
    }

    results
}

// ---------------------------------------------------------------------------
// Status checking
// ---------------------------------------------------------------------------

/// Status of an agent's configuration.
#[derive(Debug, Serialize)]
pub struct AgentConfigStatus {
    pub platform: String,
    pub slug: String,
    pub detected: bool,
    pub config_files: Vec<ConfigFileStatus>,
}

/// Why a client config differs from the expected Agent Mail entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDriftReason {
    Ok,
    MissingFile,
    MissingServerEntry,
    LegacyStdio,
    StaleHttpPath,
    WrongBearerHeader,
    WrongStartupTimeout,
    DisabledServer,
    ProjectConfigDisabled,
    DuplicateServerEntries,
    UnsupportedConfig,
}

impl ConfigDriftReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::MissingFile => "missing_file",
            Self::MissingServerEntry => "missing_server_entry",
            Self::LegacyStdio => "legacy_stdio",
            Self::StaleHttpPath => "stale_http_path",
            Self::WrongBearerHeader => "wrong_bearer_header",
            Self::WrongStartupTimeout => "wrong_startup_timeout",
            Self::DisabledServer => "disabled_server",
            Self::ProjectConfigDisabled => "project_config_disabled",
            Self::DuplicateServerEntries => "duplicate_server_entries",
            Self::UnsupportedConfig => "unsupported_config",
        }
    }
}

impl fmt::Display for ConfigDriftReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operator-facing severity for a setup drift finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDriftRisk {
    None,
    Low,
    Medium,
    High,
}

impl ConfigDriftRisk {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for ConfigDriftRisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status of a single config file.
#[derive(Debug, Serialize)]
pub struct ConfigFileStatus {
    #[serde(skip_serializing)]
    pub path: String,
    /// Whether OMP's separate active user config contributes to this file's
    /// effective-runtime drift. Kept out of JSON because the public drift
    /// reasons and remediation already describe the operator-facing result.
    #[serde(skip_serializing)]
    pub omp_active_user_config_drift: bool,
    /// Whether another OMP MCP discovery input contributes a distinct live Agent
    /// Mail alias that is not shadowed by the canonical primary entry.
    #[serde(skip_serializing)]
    pub omp_mcp_alias_drift: bool,
    /// Whether OMP's effective settings authority either excludes project MCP
    /// sources or could not be evaluated safely.
    #[serde(skip_serializing)]
    pub omp_settings_config_drift: bool,
    /// Exact file contents evaluated for this verdict. Startup self-heal uses
    /// these non-serialized observations to bind a healthy status result to
    /// the fingerprint it caches, including OMP's separate user authority.
    #[serde(skip_serializing)]
    pub status_observations: Vec<ConfigStatusFileObservation>,
    #[serde(rename = "path")]
    pub redacted_path: String,
    pub exists: bool,
    pub has_server_entry: bool,
    pub url_matches: bool,
    pub expected_url: String,
    pub actual_url: Option<String>,
    pub entry_locations: Vec<String>,
    pub current_entry: Option<Value>,
    pub expected_entry: Value,
    pub drift_reasons: Vec<ConfigDriftReason>,
    pub primary_drift_reason: ConfigDriftReason,
    pub risk: ConfigDriftRisk,
    pub remediation: String,
}

/// A content witness captured by the same read that produced setup status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStatusFileObservation {
    /// Display form of the exact path read by status.
    pub path: String,
    /// Whether that read observed a regular file.
    pub exists: bool,
    /// SHA-256 of the UTF-8 bytes status parsed, when a regular file was read.
    pub content_sha256: Option<String>,
}

impl ConfigFileStatus {
    /// Treat this file's current URL as acceptable after a caller-specific override.
    pub fn mark_url_matches(&mut self) {
        self.url_matches = true;
        self.drift_reasons
            .retain(|reason| *reason != ConfigDriftReason::StaleHttpPath);
        self.refresh_drift_summary();
    }

    fn refresh_drift_summary(&mut self) {
        self.primary_drift_reason = primary_drift_reason(&self.drift_reasons);
        self.risk = risk_for_drift_reasons(&self.drift_reasons);
        if self.primary_drift_reason == ConfigDriftReason::Ok {
            self.remediation = "no action".to_string();
        }
    }
}

/// Check config status for detected agents.
#[must_use]
pub fn check_status(params: &SetupParams) -> Vec<AgentConfigStatus> {
    let platforms = params
        .agents
        .clone()
        .unwrap_or_else(|| AgentPlatform::ALL.to_vec());
    let url = params.server_url();

    let mut statuses = Vec::new();

    for platform in &platforms {
        let mut actions = platform.config_actions(params);
        if *platform == AgentPlatform::Omp
            && actions.is_empty()
            && omp_active_user_config_path(params).is_err()
        {
            // Planning deliberately exposes no writable OMP action when the
            // active user authority is unresolved. Status still needs a
            // non-writable diagnostic target so it can report the failure
            // instead of returning an empty, false-green platform result.
            actions.push(platform.omp_project_action(&url, &params.token, &params.project_dir));
        }
        let mut file_statuses = Vec::new();

        for action in &actions {
            // Skip hooks for status check
            if matches!(action.content, ConfigContent::HooksMerge { .. }) {
                continue;
            }
            file_statuses.push(config_file_status_for_action(action, params, &url));
        }

        statuses.push(AgentConfigStatus {
            platform: platform.display_name().to_string(),
            slug: platform.slug().to_string(),
            detected: false, // caller fills this from detect_installed_agents
            config_files: file_statuses,
        });
    }

    statuses
}

#[derive(Debug)]
struct ConfigContentAnalysis {
    has_server_entry: bool,
    url_matches: bool,
    actual_url: Option<String>,
    entry_locations: Vec<String>,
    current_entry: Option<Value>,
    drift_reasons: Vec<ConfigDriftReason>,
}

struct OmpActiveUserConfigInspection {
    path: PathBuf,
    observation: ConfigStatusFileObservation,
    disabled: bool,
    unsupported: bool,
}

struct OmpMcpAliasInspection {
    observations: Vec<ConfigStatusFileObservation>,
    conflict_paths: Vec<PathBuf>,
    unsupported_paths: Vec<PathBuf>,
}

impl OmpMcpAliasInspection {
    fn has_drift(&self) -> bool {
        !self.conflict_paths.is_empty() || !self.unsupported_paths.is_empty()
    }
}

fn apply_omp_mcp_authority_drift(
    reasons: &mut Vec<ConfigDriftReason>,
    drift: &OmpMcpAliasInspection,
) {
    if !drift.unsupported_paths.is_empty() {
        push_drift_reason(reasons, ConfigDriftReason::UnsupportedConfig);
    }
    if !drift.conflict_paths.is_empty() {
        push_drift_reason(reasons, ConfigDriftReason::DuplicateServerEntries);
    }
}

struct OmpSettingsAuthorityInspection {
    observations: Vec<ConfigStatusFileObservation>,
    merged_mcp_settings: Option<Value>,
    project_config_enabled: bool,
    effective_source: Option<PathBuf>,
    unsupported_path: Option<PathBuf>,
}

impl OmpSettingsAuthorityInspection {
    fn has_drift(&self) -> bool {
        self.unsupported_path.is_some() || !self.project_config_enabled
    }
}

enum OmpSettingsFileValue {
    Missing,
    Parsed(Map<String, Value>),
    Unsupported,
}

#[derive(Clone, Copy)]
enum OmpSettingsFormat {
    Yaml,
    Json,
    Jsonc,
    Toml,
}

#[derive(Clone, Copy)]
enum OmpSettingsInvalidPolicy {
    FailClosed,
    Skip,
}

struct OmpProjectSettingsSource {
    path: PathBuf,
    format: OmpSettingsFormat,
    invalid_policy: OmpSettingsInvalidPolicy,
}

enum OmpSettingsParseError {
    Invalid,
    DynamicAuthority,
}

fn config_status_file_observation(
    path: &Path,
    content: &Result<Option<(String, u32)>, SetupError>,
) -> ConfigStatusFileObservation {
    use sha2::{Digest as _, Sha256};

    match content {
        Ok(Some((content, _))) => ConfigStatusFileObservation {
            path: path.display().to_string(),
            exists: true,
            content_sha256: Some(hex::encode(Sha256::digest(content.as_bytes()))),
        },
        Ok(None) => ConfigStatusFileObservation {
            path: path.display().to_string(),
            exists: false,
            content_sha256: None,
        },
        Err(_) => ConfigStatusFileObservation {
            path: path.display().to_string(),
            exists: path.symlink_metadata().is_ok(),
            content_sha256: None,
        },
    }
}

fn config_file_status_for_action(
    action: &ConfigAction,
    params: &SetupParams,
    expected_url: &str,
) -> ConfigFileStatus {
    let home = params.home_dir_override.clone().or_else(dirs::home_dir);
    let expected_entry =
        redact_value_for_status(expected_entry_for_action(action), home.as_deref());
    let expected_auth = expected_authorization_for_action(action, &params.token);
    let expected_timeout = expected_startup_timeout_for_action(action);
    let redacted_path = redact_path_for_status(&action.file_path, home.as_deref());
    let omp_user_config_inspection = if action.platform == AgentPlatform::Omp
        && (params.skip_user_config || omp_active_user_config_path(params).is_err())
    {
        Some(inspect_omp_active_user_config(params))
    } else {
        None
    };
    let omp_user_config_drift = omp_user_config_inspection
        .as_ref()
        .filter(|inspection| inspection.disabled || inspection.unsupported);
    let omp_mcp_alias_inspection = (action.platform == AgentPlatform::Omp)
        .then(|| inspect_omp_mcp_aliases(params, &action.file_path));
    let omp_mcp_alias_drift = omp_mcp_alias_inspection
        .as_ref()
        .filter(|inspection| inspection.has_drift());
    let omp_settings_inspection =
        if action.platform == AgentPlatform::Omp && params.skip_user_config {
            Some(inspect_omp_settings_authority(params))
        } else {
            None
        };
    let omp_settings_drift = omp_settings_inspection
        .as_ref()
        .filter(|inspection| inspection.has_drift());

    let content = read_setup_file(&action.file_path, "config status file");
    let mut status_observations = vec![config_status_file_observation(&action.file_path, &content)];
    if let Some(inspection) = &omp_user_config_inspection {
        status_observations.push(inspection.observation.clone());
    }
    if let Some(inspection) = &omp_mcp_alias_inspection {
        status_observations.extend(inspection.observations.iter().cloned());
    }
    if let Some(inspection) = &omp_settings_inspection {
        status_observations.extend(inspection.observations.iter().cloned());
    }
    if matches!(&content, Ok(None)) {
        let mut drift_reasons = vec![ConfigDriftReason::MissingFile];
        if let Some(drift) = &omp_user_config_drift {
            apply_omp_active_user_config_drift(&mut drift_reasons, drift);
        }
        if let Some(drift) = omp_mcp_alias_drift {
            apply_omp_mcp_authority_drift(&mut drift_reasons, drift);
        }
        if let Some(drift) = omp_settings_drift {
            apply_omp_settings_authority_drift(&mut drift_reasons, drift);
        }
        return ConfigFileStatus {
            path: action.file_path.display().to_string(),
            omp_active_user_config_drift: omp_user_config_drift.is_some(),
            omp_mcp_alias_drift: omp_mcp_alias_drift.is_some(),
            omp_settings_config_drift: omp_settings_drift.is_some(),
            status_observations,
            redacted_path,
            exists: false,
            has_server_entry: false,
            url_matches: false,
            expected_url: expected_url.to_string(),
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            expected_entry,
            primary_drift_reason: primary_drift_reason(&drift_reasons),
            risk: risk_for_drift_reasons(&drift_reasons),
            remediation: setup_status_remediation_for_action(
                action,
                params,
                &drift_reasons,
                omp_user_config_drift,
                omp_mcp_alias_drift,
                omp_settings_drift,
            ),
            drift_reasons,
        };
    }

    let mut analysis = match &content {
        Ok(Some((content, _))) => analyze_config_content(
            &action.file_path,
            content,
            expected_url,
            expected_auth.as_deref(),
            expected_timeout,
            home.as_deref(),
        ),
        Ok(None) => unreachable!("missing setup files return above"),
        Err(_) => ConfigContentAnalysis {
            has_server_entry: false,
            url_matches: false,
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            drift_reasons: vec![ConfigDriftReason::UnsupportedConfig],
        },
    };

    if action.platform == AgentPlatform::Omp
        && let Ok(Some((content, _))) = &content
    {
        let is_active_user_config =
            omp_active_user_config_path(params).is_ok_and(|path| path == action.file_path);
        apply_omp_config_contract(
            content,
            &mut analysis,
            expected_url,
            expected_auth.as_deref(),
            home.as_deref(),
            is_active_user_config,
        );
    }
    if let Some(drift) = &omp_user_config_drift {
        apply_omp_active_user_config_drift(&mut analysis.drift_reasons, drift);
    }
    if let Some(drift) = omp_mcp_alias_drift {
        apply_omp_mcp_authority_drift(&mut analysis.drift_reasons, drift);
    }
    if let Some(drift) = omp_settings_drift {
        apply_omp_settings_authority_drift(&mut analysis.drift_reasons, drift);
    }

    ConfigFileStatus {
        path: action.file_path.display().to_string(),
        omp_active_user_config_drift: omp_user_config_drift.is_some(),
        omp_mcp_alias_drift: omp_mcp_alias_drift.is_some(),
        omp_settings_config_drift: omp_settings_drift.is_some(),
        status_observations,
        redacted_path,
        exists: true,
        has_server_entry: analysis.has_server_entry,
        url_matches: analysis.url_matches,
        expected_url: expected_url.to_string(),
        actual_url: analysis.actual_url,
        entry_locations: analysis.entry_locations,
        current_entry: analysis.current_entry,
        expected_entry,
        primary_drift_reason: primary_drift_reason(&analysis.drift_reasons),
        risk: risk_for_drift_reasons(&analysis.drift_reasons),
        remediation: setup_status_remediation_for_action(
            action,
            params,
            &analysis.drift_reasons,
            omp_user_config_drift,
            omp_mcp_alias_drift,
            omp_settings_drift,
        ),
        drift_reasons: analysis.drift_reasons,
    }
}

// `agent-mail` is a documented historical Agent Mail key and OMP accepts
// arbitrary server names. Treat it as migration input here: OMP setup/status
// converges all three spellings onto one canonical native entry.
const OMP_SERVER_ALIASES: [&str; 3] = ["mcp-agent-mail", "mcp_agent_mail", "agent-mail"];

fn apply_omp_config_contract(
    content: &str,
    analysis: &mut ConfigContentAnalysis,
    expected_url: &str,
    expected_auth: Option<&str>,
    home: Option<&Path>,
    is_active_user_config: bool,
) {
    let Ok(doc) = serde_json::from_str::<Value>(content) else {
        return;
    };
    let Some(object) = doc.as_object() else {
        return;
    };

    apply_omp_native_server_contract(
        object,
        analysis,
        expected_url,
        expected_auth,
        home,
        &OMP_SERVER_ALIASES,
    );
    let legacy_entry_exists = ["servers", "mcp", "mcp_servers"].iter().any(|container| {
        object
            .get(*container)
            .and_then(Value::as_object)
            .is_some_and(|servers| {
                OMP_SERVER_ALIASES
                    .iter()
                    .any(|name| servers.contains_key(*name))
            })
    });
    if analysis.has_server_entry && legacy_entry_exists {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::DuplicateServerEntries,
        );
    }
    if is_active_user_config {
        // OMP reads these runtime lists only from the active user mcp.json.
        // Project files may contain similarly named keys, but they have no
        // effect on discovery. The lists are exact server-name sets: a deny
        // for the historical underscore alias does not disable the canonical
        // hyphenated entry.
        apply_omp_disabled_servers_contract(object, analysis, &OMP_SERVER_ALIASES[..1]);
    }
}

fn apply_omp_native_server_contract(
    object: &Map<String, Value>,
    analysis: &mut ConfigContentAnalysis,
    expected_url: &str,
    expected_auth: Option<&str>,
    home: Option<&Path>,
    aliases: &[&str],
) {
    match object.get("mcpServers") {
        None => {
            // Generic status parsing recognizes historical containers, but
            // OMP loads only its native `mcpServers` object. Do not report a
            // legacy-only entry as runnable.
            reset_omp_server_analysis(analysis, ConfigDriftReason::MissingServerEntry);
        }
        Some(Value::Object(servers)) => {
            let native_entries = aliases
                .iter()
                .filter_map(|name| servers.get(*name).map(|entry| (*name, entry)))
                .collect::<Vec<_>>();
            if native_entries.is_empty() {
                reset_omp_server_analysis(analysis, ConfigDriftReason::MissingServerEntry);
            } else {
                apply_omp_native_entries_contract(
                    &native_entries,
                    analysis,
                    expected_url,
                    expected_auth,
                    home,
                );
            }
        }
        Some(_) => reset_omp_server_analysis(analysis, ConfigDriftReason::UnsupportedConfig),
    }
}

fn reset_omp_server_analysis(analysis: &mut ConfigContentAnalysis, reason: ConfigDriftReason) {
    analysis.has_server_entry = false;
    analysis.url_matches = false;
    analysis.actual_url = None;
    analysis.current_entry = None;
    analysis
        .drift_reasons
        .retain(|item| *item == ConfigDriftReason::DuplicateServerEntries);
    push_drift_reason(&mut analysis.drift_reasons, reason);
}

fn apply_omp_native_entries_contract(
    native_entries: &[(&str, &Value)],
    analysis: &mut ConfigContentAnalysis,
    expected_url: &str,
    expected_auth: Option<&str>,
    home: Option<&Path>,
) {
    // Generic JSON analysis deliberately recognizes historical MCP
    // containers for migration diagnostics. OMP does not load those
    // containers, so only its native entries may satisfy URL/auth health.
    // Keep duplicate-location evidence, but recompute every active-entry
    // verdict from `mcpServers`.
    analysis.has_server_entry = true;
    analysis.url_matches = native_entries.iter().any(|(_, entry)| {
        json_entry_url(entry).is_some_and(|url| urls_match_for_status(url, expected_url))
    });
    analysis.actual_url = native_entries
        .iter()
        .find_map(|(_, entry)| json_entry_url(entry).map(str::to_string));
    analysis.current_entry = native_entries.first().map(|(server_name, entry)| {
        json!({
            "container": "mcpServers",
            "server_name": server_name,
            "entry": redact_value_for_status((*entry).clone(), home),
        })
    });
    analysis.entry_locations = native_entries
        .iter()
        .map(|(server_name, _)| format!("mcpServers.{server_name}"))
        .collect();
    analysis
        .drift_reasons
        .retain(|reason| *reason == ConfigDriftReason::DuplicateServerEntries);
    if native_entries.len() > 1 {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::DuplicateServerEntries,
        );
    } else if native_entries[0].0 != "mcp-agent-mail" {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::UnsupportedConfig,
        );
    }

    if !analysis.url_matches {
        if native_entries
            .iter()
            .any(|(_, entry)| json_entry_has_legacy_stdio(entry))
        {
            push_drift_reason(&mut analysis.drift_reasons, ConfigDriftReason::LegacyStdio);
        } else if analysis.actual_url.is_some() {
            push_drift_reason(
                &mut analysis.drift_reasons,
                ConfigDriftReason::StaleHttpPath,
            );
        } else {
            push_drift_reason(
                &mut analysis.drift_reasons,
                ConfigDriftReason::UnsupportedConfig,
            );
        }
    }
    let has_runtime_auth_override = native_entries.iter().any(|(_, entry)| {
        entry.as_object().is_some_and(|entry| {
            ["auth", "oauth"]
                .iter()
                .any(|key| entry.get(*key).is_some_and(|value| !value.is_null()))
        })
    });
    let auth_matches = !has_runtime_auth_override
        && expected_auth.map_or_else(
            || {
                native_entries
                    .iter()
                    .all(|(_, entry)| json_entry_authorization(entry).is_none())
            },
            |expected| {
                native_entries
                    .iter()
                    .any(|(_, entry)| json_entry_authorization(entry) == Some(expected))
            },
        );
    if !auth_matches {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::WrongBearerHeader,
        );
    }

    for (_, entry) in native_entries {
        let Some(entry) = entry.as_object() else {
            push_drift_reason(
                &mut analysis.drift_reasons,
                ConfigDriftReason::UnsupportedConfig,
            );
            continue;
        };
        if let Some(enabled) = entry.get("enabled") {
            let recognized_false_string = enabled.as_str().is_some_and(|value| {
                matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "0")
            });
            if enabled == &Value::Bool(false) || recognized_false_string {
                push_drift_reason(
                    &mut analysis.drift_reasons,
                    ConfigDriftReason::DisabledServer,
                );
            }
            if !enabled.is_boolean() && !recognized_false_string {
                push_drift_reason(
                    &mut analysis.drift_reasons,
                    ConfigDriftReason::UnsupportedConfig,
                );
            }
        }
    }
}

fn apply_omp_disabled_servers_contract(
    object: &Map<String, Value>,
    analysis: &mut ConfigContentAnalysis,
    aliases: &[&str],
) {
    let (disabled, unsupported) = inspect_omp_disabled_servers_contract(object, aliases);
    if unsupported {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::UnsupportedConfig,
        );
    }
    if disabled {
        push_drift_reason(
            &mut analysis.drift_reasons,
            ConfigDriftReason::DisabledServer,
        );
    }
}

fn inspect_omp_disabled_servers_contract(
    object: &Map<String, Value>,
    aliases: &[&str],
) -> (bool, bool) {
    match object.get("disabledServers") {
        None => (false, false),
        Some(Value::Array(servers)) => (
            servers
                .iter()
                .any(|value| value.as_str().is_some_and(|name| aliases.contains(&name))),
            servers.iter().any(|value| !value.is_string()),
        ),
        Some(_) => (false, true),
    }
}

fn inspect_omp_active_user_config(params: &SetupParams) -> OmpActiveUserConfigInspection {
    let path = match omp_active_user_config_path(params) {
        Ok(path) => path,
        Err(_) => {
            let path = params
                .omp_user_config_path_override
                .clone()
                .unwrap_or_else(|| PathBuf::from("<unresolved-omp-user-config>"));
            return OmpActiveUserConfigInspection {
                observation: ConfigStatusFileObservation {
                    path: path.display().to_string(),
                    exists: false,
                    content_sha256: None,
                },
                path,
                disabled: false,
                unsupported: true,
            };
        }
    };
    let content = read_setup_file(&path, "OMP active-profile user config");
    let observation = config_status_file_observation(&path, &content);
    let content = match content {
        Ok(None) => {
            return OmpActiveUserConfigInspection {
                path,
                observation,
                disabled: false,
                unsupported: false,
            };
        }
        Ok(Some((content, _))) => content,
        Err(_) => {
            return OmpActiveUserConfigInspection {
                path,
                observation,
                disabled: false,
                unsupported: true,
            };
        }
    };
    let object = match serde_json::from_str::<Value>(&content) {
        Ok(Value::Object(object)) => object,
        Ok(_) | Err(_) => {
            return OmpActiveUserConfigInspection {
                path,
                observation,
                disabled: false,
                unsupported: true,
            };
        }
    };
    let (disabled, unsupported) =
        inspect_omp_disabled_servers_contract(&object, &OMP_SERVER_ALIASES[..1]);
    OmpActiveUserConfigInspection {
        path,
        observation,
        disabled,
        unsupported,
    }
}

#[derive(Clone, PartialEq)]
struct OmpMcpConnection {
    transport: Option<String>,
    command: Option<String>,
    args: Option<Value>,
    env: Option<Value>,
    cwd: Option<String>,
    url: Option<String>,
    headers: Option<Value>,
    auth: Option<Value>,
    oauth: Option<Value>,
    request_id_format: Option<String>,
    invalid_endpoint_shape: bool,
}

impl OmpMcpConnection {
    fn effective_transport(&self) -> &str {
        self.transport.as_deref().unwrap_or_else(|| {
            if self.command.is_some() {
                "stdio"
            } else if self.url.is_some() {
                "http"
            } else {
                "stdio"
            }
        })
    }

    fn is_runtime_valid(&self) -> bool {
        if self.invalid_endpoint_shape || (self.command.is_none() && self.url.is_none()) {
            return false;
        }
        match self.effective_transport() {
            "stdio" => self.command.is_some(),
            "http" | "sse" => self.url.is_some(),
            _ => false,
        }
    }

    fn is_equivalent_to(&self, other: &Self) -> bool {
        if self.auth != other.auth
            || self.oauth != other.oauth
            || self.request_id_format.as_deref().unwrap_or("number")
                != other.request_id_format.as_deref().unwrap_or("number")
            || self.effective_transport() != other.effective_transport()
        {
            return false;
        }
        if self.effective_transport() == "stdio" {
            self.command == other.command
                && self.args == other.args
                && self.env == other.env
                && self.cwd == other.cwd
        } else {
            self.url == other.url && self.headers == other.headers
        }
    }
}

#[derive(Clone)]
struct OmpMcpCandidate {
    name: String,
    enabled: Option<bool>,
    connection: OmpMcpConnection,
    source_path: PathBuf,
    project_level: bool,
}

fn omp_mcp_optional_string(
    entry: &Map<String, Value>,
    key: &str,
    invalid_endpoint_shape: &mut bool,
) -> Option<String> {
    match entry.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(_) => {
            *invalid_endpoint_shape = true;
            None
        }
    }
}

fn omp_mcp_enabled_value(
    entry: &Map<String, Value>,
    format: OmpMcpAuthorityFormat,
) -> Option<bool> {
    match entry.get("enabled") {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(value)) if format == OmpMcpAuthorityFormat::Native => {
            match value.to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

fn omp_mcp_json_connection(
    entry: &Map<String, Value>,
    format: OmpMcpAuthorityFormat,
    provider: OmpMcpAuthorityProvider,
) -> OmpMcpConnection {
    let mut invalid_endpoint_shape = false;
    let mut command = if format == OmpMcpAuthorityFormat::OpenCodeJsonc
        && entry.get("command").is_some_and(Value::is_array)
    {
        None
    } else {
        omp_mcp_optional_string(entry, "command", &mut invalid_endpoint_shape)
    };
    let mut args = entry.get("args").cloned();
    let mut env = entry.get("env").cloned();
    let mut cwd = None;
    let url = omp_mcp_optional_string(entry, "url", &mut invalid_endpoint_shape);
    let headers = entry.get("headers").cloned();
    let mut auth = None;
    let mut oauth = None;
    let mut request_id_format = None;
    let transport = match format {
        OmpMcpAuthorityFormat::VsCodeJson => entry
            .get("transport")
            .and_then(Value::as_str)
            .filter(|transport| matches!(*transport, "stdio" | "sse" | "http"))
            .map(str::to_string),
        OmpMcpAuthorityFormat::OpenCodeJsonc => match entry.get("type").and_then(Value::as_str) {
            Some("local") => Some("stdio".to_string()),
            Some("remote") => Some("http".to_string()),
            _ => None,
        },
        OmpMcpAuthorityFormat::CodexToml => {
            cwd = omp_mcp_optional_string(entry, "cwd", &mut invalid_endpoint_shape);
            if entry.get("url").is_some() {
                Some("http".to_string())
            } else if entry.get("command").is_some() {
                Some("stdio".to_string())
            } else {
                None
            }
        }
        _ => entry
            .get("type")
            .and_then(Value::as_str)
            .and_then(|transport| {
                if matches!(
                    provider,
                    OmpMcpAuthorityProvider::Cursor | OmpMcpAuthorityProvider::Gemini
                ) && !matches!(transport, "stdio" | "sse" | "http")
                {
                    None
                } else {
                    Some(transport.to_string())
                }
            }),
    };

    match format {
        OmpMcpAuthorityFormat::Native | OmpMcpAuthorityFormat::Standalone => {
            cwd = omp_mcp_optional_string(entry, "cwd", &mut invalid_endpoint_shape);
            auth = entry.get("auth").cloned();
            oauth = entry.get("oauth").cloned();
            request_id_format = entry
                .get("requestIdFormat")
                .and_then(Value::as_str)
                .filter(|value| matches!(*value, "number" | "string"))
                .map(str::to_string);
        }
        OmpMcpAuthorityFormat::CodexToml => {
            env = entry.get("env").cloned();
        }
        OmpMcpAuthorityFormat::OpenCodeJsonc => {
            if let Some(Value::Array(command_parts)) = entry.get("command") {
                command = command_parts
                    .first()
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                if command.is_none() || command_parts[1..].iter().any(|value| !value.is_string()) {
                    invalid_endpoint_shape = true;
                }
                let mut combined = command_parts[1..].to_vec();
                if let Some(Value::Array(configured_args)) = &args {
                    combined.extend(configured_args.iter().cloned());
                } else if args.is_some() {
                    invalid_endpoint_shape = true;
                }
                args = (!combined.is_empty()).then_some(Value::Array(combined));
            }
            env = entry
                .get("environment")
                .filter(|value| value.is_object())
                .cloned()
                .or_else(|| entry.get("env").filter(|value| value.is_object()).cloned());
        }
        OmpMcpAuthorityFormat::StandardJson
        | OmpMcpAuthorityFormat::VsCodeJson
        | OmpMcpAuthorityFormat::Unsupported => {}
    }

    OmpMcpConnection {
        transport,
        command,
        args,
        env,
        cwd,
        url,
        headers,
        auth,
        oauth,
        request_id_format,
        invalid_endpoint_shape,
    }
}

fn omp_mcp_candidate_from_entry(
    name: &str,
    entry: &Value,
    source: &OmpMcpAuthoritySource,
) -> OmpMcpCandidate {
    let empty = Map::new();
    let object = entry.as_object().unwrap_or(&empty);
    let mut connection = omp_mcp_json_connection(object, source.format, source.provider);
    if !entry.is_object() {
        connection.invalid_endpoint_shape = true;
    }
    OmpMcpCandidate {
        name: name.to_string(),
        enabled: omp_mcp_enabled_value(object, source.format),
        connection,
        source_path: source.path.clone(),
        project_level: source.project_level,
    }
}

fn omp_mcp_source_has_dynamic_inputs(content: &str, format: OmpMcpAuthorityFormat) -> bool {
    match format {
        OmpMcpAuthorityFormat::OpenCodeJsonc => normalize_jsonc(content)
            .is_ok_and(|normalized| normalized.contains("{env:") || normalized.contains("{file:")),
        OmpMcpAuthorityFormat::CodexToml => toml::from_str::<toml::Value>(content)
            .ok()
            .and_then(|document| document.get("mcp_servers").cloned())
            .and_then(|servers| servers.as_table().cloned())
            .is_some_and(|servers| {
                servers.values().any(|entry| {
                    entry.as_table().is_some_and(|entry| {
                        ["env_vars", "env_http_headers", "bearer_token_env_var"]
                            .iter()
                            .any(|key| entry.contains_key(*key))
                    })
                })
            }),
        OmpMcpAuthorityFormat::Unsupported => true,
        _ => content.contains("${"),
    }
}

fn omp_mcp_parse_regular_source(
    content: &str,
    source: &OmpMcpAuthoritySource,
) -> Vec<OmpMcpCandidate> {
    let document = match source.format {
        OmpMcpAuthorityFormat::CodexToml => {
            let Ok(document) = toml::from_str::<toml::Value>(content) else {
                return Vec::new();
            };
            let Ok(document) = serde_json::to_value(document) else {
                return Vec::new();
            };
            document
        }
        OmpMcpAuthorityFormat::OpenCodeJsonc | OmpMcpAuthorityFormat::Unsupported => {
            return Vec::new();
        }
        _ => {
            let Ok(document) = serde_json::from_str::<Value>(content) else {
                return Vec::new();
            };
            document
        }
    };
    let servers = match source.format {
        OmpMcpAuthorityFormat::CodexToml => document.get("mcp_servers"),
        OmpMcpAuthorityFormat::VsCodeJson => document.get("mcp").and_then(|mcp| mcp.get("servers")),
        _ => document.get("mcpServers"),
    };
    let Some(servers) = servers.and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .map(|(name, entry)| {
            let mut candidate = omp_mcp_candidate_from_entry(name, entry, source);
            if source.format == OmpMcpAuthorityFormat::CodexToml {
                candidate.connection.headers = entry.get("http_headers").cloned();
            }
            candidate
        })
        .collect()
}

fn merge_omp_mcp_config_record(base: &mut Value, incoming: Value) {
    match (base, incoming) {
        (Value::Object(base), Value::Object(incoming)) => {
            for (key, incoming) in incoming {
                if let Some(base) = base.get_mut(&key) {
                    merge_omp_mcp_config_record(base, incoming);
                } else {
                    base.insert(key, incoming);
                }
            }
        }
        (base, incoming) => *base = incoming,
    }
}

fn parse_omp_opencode_layer(content: &str) -> Option<Map<String, Value>> {
    let normalized = normalize_jsonc(content).ok()?;
    serde_json::from_str::<Value>(&normalized)
        .ok()?
        .get("mcp")?
        .as_object()
        .cloned()
}

fn inspect_omp_runtime_server_lists(
    params: &SetupParams,
) -> Result<
    (
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    ),
    PathBuf,
> {
    let path = omp_active_user_config_path(params).map_err(|_| {
        params
            .omp_user_config_path_override
            .clone()
            .unwrap_or_else(|| PathBuf::from("<unresolved-omp-user-config>"))
    })?;
    let content =
        read_setup_file(&path, "OMP active-profile MCP runtime lists").map_err(|_| path.clone())?;
    let Some((content, _)) = content else {
        return Ok(Default::default());
    };
    let Value::Object(document) =
        serde_json::from_str::<Value>(&content).map_err(|_| path.clone())?
    else {
        return Err(path);
    };
    let read_list = |key: &str| -> Option<std::collections::HashSet<String>> {
        match document.get(key) {
            None => Some(std::collections::HashSet::new()),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect(),
            Some(_) => None,
        }
    };
    let disabled = read_list("disabledServers").ok_or_else(|| path.clone())?;
    let enabled = read_list("enabledServers").ok_or_else(|| path.clone())?;
    Ok((disabled, enabled))
}

fn inspect_omp_mcp_aliases(params: &SetupParams, action_path: &Path) -> OmpMcpAliasInspection {
    let mut inspection = OmpMcpAliasInspection {
        observations: Vec::new(),
        conflict_paths: Vec::new(),
        unsupported_paths: Vec::new(),
    };

    let settings = inspect_omp_settings_authority(params);
    inspection
        .observations
        .extend(settings.observations.clone());
    if let Some(path) = settings.unsupported_path {
        inspection.unsupported_paths.push(path);
    }
    let project_config_enabled = settings.project_config_enabled;

    let (disabled_servers, forced_enabled) = inspect_omp_runtime_server_lists(params)
        .unwrap_or_else(|path| {
            inspection.unsupported_paths.push(path);
            Default::default()
        });

    let mut candidates = Vec::new();
    let mut claude_project_selected = false;
    let mut claude_user_selected = false;
    let mut open_code_merged = Map::<String, Value>::new();
    let mut open_code_sources = std::collections::HashMap::<String, (PathBuf, bool)>::new();

    let flush_open_code =
        |candidates: &mut Vec<OmpMcpCandidate>,
         merged: &mut Map<String, Value>,
         source_paths: &mut std::collections::HashMap<String, (PathBuf, bool)>| {
            for (name, entry) in std::mem::take(merged) {
                let Some((path, project_level)) = source_paths.remove(&name) else {
                    continue;
                };
                let source = OmpMcpAuthoritySource {
                    path,
                    format: OmpMcpAuthorityFormat::OpenCodeJsonc,
                    provider: OmpMcpAuthorityProvider::OpenCode,
                    project_level,
                };
                candidates.push(omp_mcp_candidate_from_entry(&name, &entry, &source));
            }
        };

    let mut in_open_code = false;
    for source in omp_mcp_authority_sources(params) {
        if in_open_code && source.provider != OmpMcpAuthorityProvider::OpenCode {
            flush_open_code(
                &mut candidates,
                &mut open_code_merged,
                &mut open_code_sources,
            );
            in_open_code = false;
        }

        if source.format == OmpMcpAuthorityFormat::Unsupported {
            inspection.unsupported_paths.push(source.path);
            continue;
        }
        let content = read_setup_file(&source.path, "OMP MCP discovery authority");
        if source.path != action_path {
            inspection
                .observations
                .push(config_status_file_observation(&source.path, &content));
        }
        let content = match content {
            Ok(None) => continue,
            Ok(Some((content, _))) => content,
            Err(_) => {
                inspection.unsupported_paths.push(source.path);
                continue;
            }
        };
        if omp_mcp_source_has_dynamic_inputs(&content, source.format) {
            inspection.unsupported_paths.push(source.path);
            continue;
        }

        if source.provider == OmpMcpAuthorityProvider::OpenCode {
            in_open_code = true;
            if let Some(layer) = parse_omp_opencode_layer(&content) {
                for (name, entry) in layer {
                    if !entry.is_object() {
                        continue;
                    }
                    if let Some(existing) = open_code_merged.get_mut(&name) {
                        merge_omp_mcp_config_record(existing, entry);
                    } else {
                        open_code_merged.insert(name.clone(), entry);
                    }
                    open_code_sources.insert(name, (source.path.clone(), source.project_level));
                }
            }
            continue;
        }

        let parsed = omp_mcp_parse_regular_source(&content, &source);
        match source.provider {
            OmpMcpAuthorityProvider::ClaudeProject => {
                if !claude_project_selected && !parsed.is_empty() {
                    candidates.extend(parsed);
                    claude_project_selected = true;
                }
            }
            OmpMcpAuthorityProvider::ClaudeUser => {
                if !claude_user_selected && !parsed.is_empty() {
                    candidates.extend(parsed);
                    claude_user_selected = true;
                }
            }
            _ => candidates.extend(parsed),
        }
    }
    if in_open_code {
        flush_open_code(
            &mut candidates,
            &mut open_code_merged,
            &mut open_code_sources,
        );
    }

    let mut seen_names = std::collections::HashSet::new();
    let mut surviving = Vec::<OmpMcpCandidate>::new();
    for candidate in candidates {
        if candidate.project_level && !project_config_enabled {
            continue;
        }
        let suppressed = disabled_servers.contains(&candidate.name)
            || (candidate.enabled == Some(false) && !forced_enabled.contains(&candidate.name));
        if suppressed {
            seen_names.insert(candidate.name);
            continue;
        }
        if !seen_names.insert(candidate.name.clone()) {
            continue;
        }
        if surviving
            .iter()
            .any(|existing| existing.connection.is_equivalent_to(&candidate.connection))
        {
            continue;
        }
        surviving.push(candidate);
    }

    for candidate in surviving {
        if OMP_SERVER_ALIASES[1..].contains(&candidate.name.as_str())
            && candidate.connection.is_runtime_valid()
            && candidate.source_path != action_path
            && !inspection.conflict_paths.contains(&candidate.source_path)
        {
            inspection.conflict_paths.push(candidate.source_path);
        }
    }
    inspection.unsupported_paths.sort();
    inspection.unsupported_paths.dedup();
    inspection
}

fn normalize_jsonc(content: &str) -> Result<String, ()> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let bytes = content.as_bytes();
    let mut without_comments = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            without_comments.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            without_comments.push(byte);
            index += 1;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            without_comments.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                without_comments.push(b' ');
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            without_comments.extend_from_slice(b"  ");
            index += 2;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    without_comments.extend_from_slice(b"  ");
                    index += 2;
                    closed = true;
                    break;
                }
                without_comments.push(if matches!(bytes[index], b'\n' | b'\r') {
                    bytes[index]
                } else {
                    b' '
                });
                index += 1;
            }
            if !closed {
                return Err(());
            }
            continue;
        }
        without_comments.push(byte);
        index += 1;
    }
    if in_string {
        return Err(());
    }

    let mut normalized = Vec::with_capacity(without_comments.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < without_comments.len() {
        let byte = without_comments[index];
        if in_string {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            normalized.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut lookahead = index + 1;
            while without_comments
                .get(lookahead)
                .is_some_and(u8::is_ascii_whitespace)
            {
                lookahead += 1;
            }
            if matches!(without_comments.get(lookahead), Some(b'}' | b']')) {
                index += 1;
                continue;
            }
        }
        normalized.push(byte);
        index += 1;
    }
    String::from_utf8(normalized).map_err(|_| ())
}

fn contains_opencode_substitution(value: &str) -> bool {
    value.contains("{env:") || value.contains("{file:")
}

fn parse_omp_settings_document(
    content: &str,
    format: OmpSettingsFormat,
) -> Result<Map<String, Value>, OmpSettingsParseError> {
    let document = match format {
        OmpSettingsFormat::Yaml => {
            let mut document = serde_norway::from_str::<serde_norway::Value>(content)
                .map_err(|_| OmpSettingsParseError::Invalid)?;
            document
                .apply_merge()
                .map_err(|_| OmpSettingsParseError::Invalid)?;
            serde_json::to_value(document).map_err(|_| OmpSettingsParseError::Invalid)?
        }
        OmpSettingsFormat::Json => {
            serde_json::from_str::<Value>(content).map_err(|_| OmpSettingsParseError::Invalid)?
        }
        OmpSettingsFormat::Jsonc => {
            // OpenCode expands these tokens before parsing. Environment values
            // are inserted without JSON escaping and referenced files can
            // change independently of the config bytes, so even a token that
            // currently appears unrelated can change the parsed document or
            // its authority after a cache entry is written. Until those
            // dependencies are fingerprinted, fail closed on every token.
            if contains_opencode_substitution(content) {
                return Err(OmpSettingsParseError::DynamicAuthority);
            }
            let normalized =
                normalize_jsonc(content).map_err(|()| OmpSettingsParseError::Invalid)?;
            match serde_json::from_str::<Value>(&normalized) {
                Ok(document) => document,
                Err(_) => return Err(OmpSettingsParseError::Invalid),
            }
        }
        OmpSettingsFormat::Toml => {
            let document = toml::from_str::<toml::Value>(content)
                .map_err(|_| OmpSettingsParseError::Invalid)?;
            serde_json::to_value(document).map_err(|_| OmpSettingsParseError::Invalid)?
        }
    };
    match document {
        Value::Null => Ok(Map::new()),
        Value::Object(root) => Ok(root),
        _ => Err(OmpSettingsParseError::Invalid),
    }
}

fn inspect_omp_settings_file(
    path: &Path,
    required: bool,
    format: OmpSettingsFormat,
    invalid_policy: OmpSettingsInvalidPolicy,
) -> (ConfigStatusFileObservation, OmpSettingsFileValue) {
    let content = read_setup_file(path, "OMP settings authority");
    let observation = config_status_file_observation(path, &content);
    let value = match content {
        Ok(None) if required => OmpSettingsFileValue::Unsupported,
        Ok(None) => OmpSettingsFileValue::Missing,
        Ok(Some((content, _))) => match parse_omp_settings_document(&content, format) {
            Ok(root) => OmpSettingsFileValue::Parsed(root),
            Err(OmpSettingsParseError::Invalid)
                if matches!(invalid_policy, OmpSettingsInvalidPolicy::Skip) =>
            {
                OmpSettingsFileValue::Missing
            }
            Err(OmpSettingsParseError::Invalid | OmpSettingsParseError::DynamicAuthority) => {
                OmpSettingsFileValue::Unsupported
            }
        },
        Err(_) if matches!(invalid_policy, OmpSettingsInvalidPolicy::Skip) => {
            OmpSettingsFileValue::Missing
        }
        Err(_) => OmpSettingsFileValue::Unsupported,
    };
    (observation, value)
}

fn deep_merge_omp_setting(base: &mut Value, override_value: Value) {
    if let (Value::Object(base), Value::Object(overrides)) = (&mut *base, &override_value) {
        for (key, value) in overrides {
            if let Some(existing) = base.get_mut(key) {
                deep_merge_omp_setting(existing, value.clone());
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = override_value;
    }
}

fn omp_project_config_is_enabled(mcp: Option<&Value>) -> bool {
    let Some(value) = mcp
        .and_then(Value::as_object)
        .and_then(|mcp| mcp.get("enableProjectConfig"))
    else {
        return true;
    };
    match value {
        // The SDK applies `?? true` before the loader's JavaScript truthiness
        // check, so an explicit null behaves like the default true.
        Value::Null => true,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn merge_omp_settings_document(
    inspection: &mut OmpSettingsAuthorityInspection,
    path: &Path,
    mut root: Map<String, Value>,
    drop_group_shadows: bool,
) {
    let Some(incoming_mcp) = root.remove("mcp") else {
        return;
    };
    if drop_group_shadows && !incoming_mcp.is_object() {
        return;
    }
    let directly_supplies_leaf = incoming_mcp
        .as_object()
        .is_some_and(|mcp| mcp.contains_key("enableProjectConfig"));
    let replaces_mcp_group = !incoming_mcp.is_object();
    if let Some(merged) = &mut inspection.merged_mcp_settings {
        deep_merge_omp_setting(merged, incoming_mcp);
    } else {
        inspection.merged_mcp_settings = Some(incoming_mcp);
    }
    inspection.project_config_enabled =
        omp_project_config_is_enabled(inspection.merged_mcp_settings.as_ref());
    if directly_supplies_leaf || replaces_mcp_group {
        inspection.effective_source = Some(path.to_path_buf());
    }
}

fn merge_omp_settings_file(
    inspection: &mut OmpSettingsAuthorityInspection,
    path: &Path,
    required: bool,
    format: OmpSettingsFormat,
    invalid_policy: OmpSettingsInvalidPolicy,
    drop_group_shadows: bool,
) -> OmpSettingsFileValue {
    let (observation, value) = inspect_omp_settings_file(path, required, format, invalid_policy);
    inspection.observations.push(observation);
    match &value {
        OmpSettingsFileValue::Parsed(root) => {
            merge_omp_settings_document(inspection, path, root.clone(), drop_group_shadows);
        }
        OmpSettingsFileValue::Unsupported => {
            inspection.unsupported_path = Some(path.to_path_buf());
        }
        OmpSettingsFileValue::Missing => {}
    }
    value
}

fn omp_project_settings_sources(params: &SetupParams) -> Vec<OmpProjectSettingsSource> {
    use OmpSettingsFormat::{Json, Jsonc, Toml, Yaml};
    use OmpSettingsInvalidPolicy::{FailClosed, Skip};

    let project = &params.project_dir;
    [
        (".omp/settings.json", Json, Skip),
        (".omp/config.yml", Yaml, FailClosed),
        (".claude/settings.json", Json, Skip),
        (".codex/config.toml", Toml, Skip),
        (".gemini/settings.json", Json, Skip),
        ("opencode.json", Jsonc, Skip),
        ("opencode.jsonc", Jsonc, Skip),
        (".opencode/opencode.json", Jsonc, Skip),
        (".opencode/opencode.jsonc", Jsonc, Skip),
        (".cursor/settings.json", Json, Skip),
    ]
    .into_iter()
    .map(|(path, format, invalid_policy)| OmpProjectSettingsSource {
        path: project.join(path),
        format,
        invalid_policy,
    })
    .collect()
}

fn inspect_omp_settings_authority(params: &SetupParams) -> OmpSettingsAuthorityInspection {
    let mut inspection = OmpSettingsAuthorityInspection {
        observations: Vec::new(),
        merged_mcp_settings: None,
        project_config_enabled: true,
        effective_source: None,
        unsupported_path: None,
    };
    let Ok((user_yml, user_yaml)) = omp_active_user_settings_paths(params) else {
        inspection.unsupported_path = Some(
            params
                .omp_user_config_path_override
                .clone()
                .unwrap_or_else(|| PathBuf::from("<unresolved-omp-user-config>")),
        );
        return inspection;
    };
    let preferred = merge_omp_settings_file(
        &mut inspection,
        &user_yml,
        false,
        OmpSettingsFormat::Yaml,
        OmpSettingsInvalidPolicy::FailClosed,
        false,
    );
    if matches!(preferred, OmpSettingsFileValue::Unsupported) {
        return inspection;
    }
    if matches!(preferred, OmpSettingsFileValue::Missing)
        && matches!(
            merge_omp_settings_file(
                &mut inspection,
                &user_yaml,
                false,
                OmpSettingsFormat::Yaml,
                OmpSettingsInvalidPolicy::FailClosed,
                false,
            ),
            OmpSettingsFileValue::Unsupported
        )
    {
        return inspection;
    }

    for source in omp_project_settings_sources(params) {
        if matches!(
            merge_omp_settings_file(
                &mut inspection,
                &source.path,
                false,
                source.format,
                source.invalid_policy,
                true,
            ),
            OmpSettingsFileValue::Unsupported
        ) {
            return inspection;
        }
    }
    for overlay in &params.omp_settings_overlay_paths {
        if matches!(
            merge_omp_settings_file(
                &mut inspection,
                overlay,
                true,
                OmpSettingsFormat::Yaml,
                OmpSettingsInvalidPolicy::FailClosed,
                false,
            ),
            OmpSettingsFileValue::Unsupported
        ) {
            break;
        }
    }
    inspection
}

fn apply_omp_settings_authority_drift(
    reasons: &mut Vec<ConfigDriftReason>,
    drift: &OmpSettingsAuthorityInspection,
) {
    if drift.unsupported_path.is_some() {
        push_drift_reason(reasons, ConfigDriftReason::UnsupportedConfig);
    } else if !drift.project_config_enabled {
        push_drift_reason(reasons, ConfigDriftReason::ProjectConfigDisabled);
    }
}

fn apply_omp_active_user_config_drift(
    reasons: &mut Vec<ConfigDriftReason>,
    drift: &OmpActiveUserConfigInspection,
) {
    if drift.unsupported {
        push_drift_reason(reasons, ConfigDriftReason::UnsupportedConfig);
    }
    if drift.disabled {
        push_drift_reason(reasons, ConfigDriftReason::DisabledServer);
    }
}

fn analyze_config_content(
    path: &Path,
    content: &str,
    expected_url: &str,
    expected_auth: Option<&str>,
    expected_startup_timeout: Option<u64>,
    home: Option<&Path>,
) -> ConfigContentAnalysis {
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        analyze_toml_config_content(
            content,
            expected_url,
            expected_auth,
            expected_startup_timeout,
            home,
        )
    } else {
        analyze_json_config_content(content, expected_url, expected_auth, home)
    }
}

/// Check whether a config file contains our server entry and the URL matches.
#[cfg(test)]
fn check_config_file(path: &Path, expected_url: &str) -> (bool, bool) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (false, false);
    };

    let analysis = analyze_config_content(path, &content, expected_url, None, None, None);
    (analysis.has_server_entry, analysis.url_matches)
}

struct JsonServerEntry<'a> {
    container: &'static str,
    server_name: &'static str,
    entry: &'a Value,
}

fn analyze_json_config_content(
    content: &str,
    expected_url: &str,
    expected_auth: Option<&str>,
    home: Option<&Path>,
) -> ConfigContentAnalysis {
    let Ok(doc) = serde_json::from_str::<Value>(content) else {
        return ConfigContentAnalysis {
            has_server_entry: false,
            url_matches: false,
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            drift_reasons: vec![ConfigDriftReason::UnsupportedConfig],
        };
    };

    if !doc.is_object() {
        return ConfigContentAnalysis {
            has_server_entry: false,
            url_matches: false,
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            drift_reasons: vec![ConfigDriftReason::UnsupportedConfig],
        };
    }

    let entries = collect_json_server_entries(&doc);
    if entries.is_empty() {
        return ConfigContentAnalysis {
            has_server_entry: false,
            url_matches: false,
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            drift_reasons: vec![ConfigDriftReason::MissingServerEntry],
        };
    }

    let mut drift_reasons = Vec::new();
    if entries.len() > 1 {
        push_drift_reason(
            &mut drift_reasons,
            ConfigDriftReason::DuplicateServerEntries,
        );
    }

    let url_matches = entries.iter().any(|entry| {
        json_entry_url(entry.entry).is_some_and(|url| urls_match_for_status(url, expected_url))
    });
    let actual_url = entries
        .iter()
        .find_map(|entry| json_entry_url(entry.entry).map(str::to_string));
    if !url_matches {
        if entries
            .iter()
            .any(|entry| json_entry_has_legacy_stdio(entry.entry))
        {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::LegacyStdio);
        } else if actual_url.is_some() {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::StaleHttpPath);
        } else {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::UnsupportedConfig);
        }
    }

    let auth_matches = expected_auth.map_or_else(
        || {
            entries
                .iter()
                .all(|entry| json_entry_authorization(entry.entry).is_none())
        },
        |expected| {
            entries
                .iter()
                .any(|entry| json_entry_authorization(entry.entry) == Some(expected))
        },
    );
    if !auth_matches {
        push_drift_reason(&mut drift_reasons, ConfigDriftReason::WrongBearerHeader);
    }

    let first = &entries[0];
    let current_entry = Some(json!({
        "container": first.container,
        "server_name": first.server_name,
        "entry": redact_value_for_status(first.entry.clone(), home),
    }));
    let entry_locations = entries
        .iter()
        .map(|entry| format!("{}.{}", entry.container, entry.server_name))
        .collect();

    ConfigContentAnalysis {
        has_server_entry: true,
        url_matches,
        actual_url,
        entry_locations,
        current_entry,
        drift_reasons,
    }
}

fn collect_json_server_entries(doc: &Value) -> Vec<JsonServerEntry<'_>> {
    let mut entries = Vec::new();
    for container in ["mcpServers", "mcp", "servers", "mcp_servers"] {
        let Some(servers) = doc.get(container).and_then(Value::as_object) else {
            continue;
        };
        for server_name in ["mcp-agent-mail", "mcp_agent_mail"] {
            let Some(entry) = servers.get(server_name) else {
                continue;
            };
            entries.push(JsonServerEntry {
                container,
                server_name,
                entry,
            });
        }
    }
    entries
}

fn json_entry_url(entry: &Value) -> Option<&str> {
    entry
        .get("url")
        .or_else(|| entry.get("httpUrl"))
        .and_then(Value::as_str)
}

fn json_entry_authorization(entry: &Value) -> Option<&str> {
    entry
        .get("headers")
        .or_else(|| entry.get("http_headers"))
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value)
        })
        .and_then(Value::as_str)
}

fn json_entry_has_legacy_stdio(entry: &Value) -> bool {
    entry.get("command").is_some()
        || entry.get("args").is_some()
        || entry
            .get("transport")
            .and_then(Value::as_str)
            .is_some_and(|transport| transport.eq_ignore_ascii_case("stdio"))
}

#[derive(Debug)]
struct TomlServerSection {
    section: String,
    entry: Map<String, Value>,
    url: Option<String>,
    authorization: Option<String>,
    startup_timeout: Option<u64>,
    legacy_stdio: bool,
}

fn analyze_toml_config_content(
    content: &str,
    expected_url: &str,
    expected_auth: Option<&str>,
    expected_startup_timeout: Option<u64>,
    home: Option<&Path>,
) -> ConfigContentAnalysis {
    let sections = collect_toml_server_sections(content);
    if sections.is_empty() {
        return ConfigContentAnalysis {
            has_server_entry: false,
            url_matches: false,
            actual_url: None,
            entry_locations: Vec::new(),
            current_entry: None,
            drift_reasons: vec![ConfigDriftReason::MissingServerEntry],
        };
    }

    let mut drift_reasons = Vec::new();
    if sections.len() > 1 {
        push_drift_reason(
            &mut drift_reasons,
            ConfigDriftReason::DuplicateServerEntries,
        );
    }

    let url_matches = sections.iter().any(|section| {
        section
            .url
            .as_deref()
            .is_some_and(|url| urls_match_for_status(url, expected_url))
    });
    let actual_url = sections.iter().find_map(|section| section.url.clone());
    if !url_matches {
        if sections.iter().any(|section| section.legacy_stdio) {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::LegacyStdio);
        } else if actual_url.is_some() {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::StaleHttpPath);
        } else {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::UnsupportedConfig);
        }
    }

    let auth_matches = expected_auth.map_or_else(
        || {
            sections
                .iter()
                .all(|section| section.authorization.is_none())
        },
        |expected| {
            sections
                .iter()
                .any(|section| section.authorization.as_deref() == Some(expected))
        },
    );
    if !auth_matches {
        push_drift_reason(&mut drift_reasons, ConfigDriftReason::WrongBearerHeader);
    }

    if let Some(expected) = expected_startup_timeout {
        let timeout_matches = sections
            .iter()
            .any(|section| section.startup_timeout == Some(expected));
        if !timeout_matches {
            push_drift_reason(&mut drift_reasons, ConfigDriftReason::WrongStartupTimeout);
        }
    }

    let first = &sections[0];
    let current_entry = Some(json!({
        "section": first.section,
        "entry": redact_value_for_status(Value::Object(first.entry.clone()), home),
    }));
    let entry_locations = sections
        .iter()
        .map(|section| section.section.clone())
        .collect();

    ConfigContentAnalysis {
        has_server_entry: true,
        url_matches,
        actual_url,
        entry_locations,
        current_entry,
        drift_reasons,
    }
}

fn collect_toml_server_sections(content: &str) -> Vec<TomlServerSection> {
    let mut sections = Vec::new();
    let mut current_index: Option<usize> = None;

    for raw_line in content.lines() {
        if let Some(section) = parse_toml_section_header(raw_line) {
            if matches!(
                section,
                "mcp_servers.mcp_agent_mail" | "mcp_servers.\"mcp-agent-mail\""
            ) {
                sections.push(TomlServerSection {
                    section: section.to_string(),
                    entry: Map::new(),
                    url: None,
                    authorization: None,
                    startup_timeout: None,
                    legacy_stdio: false,
                });
                current_index = Some(sections.len() - 1);
            } else {
                current_index = None;
            }
            continue;
        }

        let Some(index) = current_index else {
            continue;
        };
        let Some((key, value)) = parse_toml_key_value(raw_line) else {
            continue;
        };

        match key.as_str() {
            "url" | "httpUrl" => {
                if let Some(url) = value.as_str() {
                    sections[index].url = Some(url.to_string());
                }
            }
            "http_headers" => {
                sections[index].authorization = value
                    .as_object()
                    .and_then(|headers| headers.get("Authorization"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "startup_timeout_sec" => {
                sections[index].startup_timeout = value.as_u64();
            }
            "command" | "args" => {
                sections[index].legacy_stdio = true;
            }
            "transport"
                if value
                    .as_str()
                    .is_some_and(|transport| transport.eq_ignore_ascii_case("stdio")) =>
            {
                sections[index].legacy_stdio = true;
            }
            _ => {}
        }
        sections[index].entry.insert(key, value);
    }

    sections
}

fn parse_toml_key_value(line: &str) -> Option<(String, Value)> {
    let line = strip_toml_inline_comment(line);
    let (lhs, rhs) = line.split_once('=')?;
    let key = lhs.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), toml_literal_to_json_value(rhs.trim())))
}

fn toml_literal_to_json_value(value: &str) -> Value {
    if let Some(string) = parse_toml_quoted_literal(value) {
        return Value::String(string);
    }
    if let Ok(number) = value.parse::<u64>() {
        return json!(number);
    }
    if let Some(auth) = parse_toml_inline_authorization(value) {
        return json!({ "Authorization": auth });
    }
    Value::String(value.to_string())
}

fn parse_toml_quoted_literal(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(value) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        return Some(value.to_string());
    }
    value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .map(str::to_string)
}

fn parse_toml_inline_authorization(value: &str) -> Option<String> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    for part in inner.split(',') {
        let (key, raw_value) = part.split_once('=')?;
        if key.trim() == "Authorization" {
            return parse_toml_quoted_literal(raw_value.trim());
        }
    }
    None
}

fn expected_entry_for_action(action: &ConfigAction) -> Value {
    match &action.content {
        ConfigContent::JsonMerge {
            servers_key,
            server_name,
            server_value,
            ..
        } => json!({
            "container": servers_key,
            "server_name": server_name,
            "entry": server_value,
        }),
        ConfigContent::ClaudeLocalScopeMcp {
            project_path,
            server_name,
            server_value,
        } => json!({
            "container": format!("projects.{project_path}.mcpServers"),
            "server_name": server_name,
            "entry": server_value,
        }),
        ConfigContent::JsonFull(value) => value.clone(),
        ConfigContent::HooksMerge { .. } => json!({}),
        ConfigContent::TomlSection {
            section_header,
            key_values,
        } => {
            let mut entry = Map::new();
            for (key, value) in key_values {
                entry.insert(key.clone(), toml_literal_to_json_value(value));
            }
            json!({
                "section": section_header.trim_matches(['[', ']']),
                "entry": entry,
            })
        }
    }
}

fn expected_authorization_for_action(action: &ConfigAction, token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    match &action.content {
        ConfigContent::JsonMerge { server_value, .. }
        | ConfigContent::ClaudeLocalScopeMcp { server_value, .. } => {
            json_entry_authorization(server_value).map(str::to_string)
        }
        ConfigContent::TomlSection { key_values, .. } => {
            key_values.iter().find_map(|(key, value)| {
                (key == "http_headers")
                    .then(|| parse_toml_inline_authorization(value))
                    .flatten()
            })
        }
        ConfigContent::JsonFull(_) | ConfigContent::HooksMerge { .. } => None,
    }
}

fn expected_startup_timeout_for_action(action: &ConfigAction) -> Option<u64> {
    if action.platform != AgentPlatform::Codex {
        return None;
    }
    match &action.content {
        ConfigContent::TomlSection { key_values, .. } => {
            key_values.iter().find_map(|(key, value)| {
                (key == "startup_timeout_sec")
                    .then(|| value.parse::<u64>().ok())
                    .flatten()
            })
        }
        ConfigContent::JsonMerge { .. }
        | ConfigContent::ClaudeLocalScopeMcp { .. }
        | ConfigContent::JsonFull(_)
        | ConfigContent::HooksMerge { .. } => None,
    }
}

fn redact_value_for_status(value: Value, home: Option<&Path>) -> Value {
    redact_value_for_status_key(None, value, home)
}

fn redact_value_for_status_key(key: Option<&str>, value: Value, home: Option<&Path>) -> Value {
    let key_lc = key.unwrap_or_default().to_ascii_lowercase();
    match value {
        Value::String(text) => {
            if key_lc.contains("authorization")
                || key_lc.contains("token")
                || key_lc.contains("secret")
            {
                if text.trim_start().starts_with("Bearer ") {
                    Value::String("Bearer <redacted>".to_string())
                } else {
                    Value::String("<redacted>".to_string())
                }
            } else if text.trim_start().starts_with("Bearer ") {
                Value::String("Bearer <redacted>".to_string())
            } else {
                Value::String(redact_home_in_status_text(&text, home))
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_value_for_status_key(key, value, home))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let value = redact_value_for_status_key(Some(&key), value, home);
                    (key, value)
                })
                .collect(),
        ),
        other => other,
    }
}

fn redact_path_for_status(path: &Path, home: Option<&Path>) -> String {
    redact_home_in_status_text(&path.display().to_string(), home)
}

fn redact_home_in_status_text(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return text.to_string();
    };
    let home = home.display().to_string();
    if home.is_empty() || home == "/" {
        return text.to_string();
    }
    if text == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = text.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    text.replace(&prefix, "~/")
}

fn push_drift_reason(reasons: &mut Vec<ConfigDriftReason>, reason: ConfigDriftReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn primary_drift_reason(reasons: &[ConfigDriftReason]) -> ConfigDriftReason {
    const PRIORITY: &[ConfigDriftReason] = &[
        ConfigDriftReason::UnsupportedConfig,
        ConfigDriftReason::ProjectConfigDisabled,
        ConfigDriftReason::MissingFile,
        ConfigDriftReason::MissingServerEntry,
        ConfigDriftReason::DuplicateServerEntries,
        ConfigDriftReason::DisabledServer,
        ConfigDriftReason::LegacyStdio,
        ConfigDriftReason::StaleHttpPath,
        ConfigDriftReason::WrongBearerHeader,
        ConfigDriftReason::WrongStartupTimeout,
    ];
    PRIORITY
        .iter()
        .copied()
        .find(|reason| reasons.contains(reason))
        .unwrap_or(ConfigDriftReason::Ok)
}

fn risk_for_drift_reasons(reasons: &[ConfigDriftReason]) -> ConfigDriftRisk {
    if reasons.is_empty() {
        return ConfigDriftRisk::None;
    }
    if reasons.iter().any(|reason| {
        matches!(
            reason,
            ConfigDriftReason::UnsupportedConfig
                | ConfigDriftReason::DuplicateServerEntries
                | ConfigDriftReason::WrongBearerHeader
        )
    }) {
        return ConfigDriftRisk::High;
    }
    if reasons
        .iter()
        .all(|reason| *reason == ConfigDriftReason::WrongStartupTimeout)
    {
        return ConfigDriftRisk::Low;
    }
    ConfigDriftRisk::Medium
}

fn setup_status_remediation(
    action: &ConfigAction,
    params: &SetupParams,
    reasons: &[ConfigDriftReason],
) -> String {
    if reasons.is_empty() {
        return "no action".to_string();
    }

    let args = setup_status_command_args(action, params, params.skip_user_config);
    let dry_run = format!("am setup run --dry-run {args}");
    let fix = format!("am setup run --yes {args}");

    if reasons.contains(&ConfigDriftReason::UnsupportedConfig) {
        return format!("inspect unsupported config, then {dry_run}; {fix}");
    }
    if reasons.contains(&ConfigDriftReason::WrongBearerHeader) {
        return format!(
            "stale bearer token suspected: verify HTTP_BEARER_TOKEN/config.env token source, then {dry_run}; {fix}"
        );
    }
    if reasons.contains(&ConfigDriftReason::DuplicateServerEntries) {
        return format!(
            "server-name alias conflict (`mcp-agent-mail` vs `mcp_agent_mail`): {dry_run}; {fix}"
        );
    }
    format!("{dry_run}; {fix}")
}

fn setup_status_command_args(
    action: &ConfigAction,
    params: &SetupParams,
    skip_user_config: bool,
) -> String {
    let home = params.home_dir_override.clone().or_else(dirs::home_dir);
    let project_dir = redact_path_for_status(&params.project_dir, home.as_deref());
    format!(
        "--agent {} --host {} --port {} --path {} --project-dir {}{}{}",
        action.platform.slug(),
        shell_quote_status_arg(&params.host),
        params.port,
        shell_quote_status_arg(&params.path),
        shell_quote_status_path(&project_dir),
        if skip_user_config {
            " --no-user-config"
        } else {
            ""
        },
        if params.skip_hooks { " --no-hooks" } else { "" }
    )
}

fn shell_quote_status_arg(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn shell_quote_status_path(value: &str) -> String {
    if value == "~" {
        return value.to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        // Keep only the tilde unquoted so the shell expands the redacted home;
        // quote the complete suffix as one argument.
        return format!("~/{}", shell_quote_status_arg(rest));
    }
    shell_quote_status_arg(value)
}

fn setup_status_remediation_for_action(
    action: &ConfigAction,
    params: &SetupParams,
    reasons: &[ConfigDriftReason],
    omp_user_config_drift: Option<&OmpActiveUserConfigInspection>,
    omp_mcp_alias_drift: Option<&OmpMcpAliasInspection>,
    omp_settings_drift: Option<&OmpSettingsAuthorityInspection>,
) -> String {
    if omp_user_config_drift.is_none()
        && omp_mcp_alias_drift.is_none()
        && omp_settings_drift.is_none()
    {
        return setup_status_remediation(action, params, reasons);
    }
    let home = params.home_dir_override.clone().or_else(dirs::home_dir);
    let args = setup_status_command_args(action, params, false);
    let dry_run = format!("am setup run --dry-run {args}");
    let fix = format!("am setup run --yes {args}");

    let mut unsupported = Vec::new();
    if let Some(path) = omp_settings_drift.and_then(|drift| drift.unsupported_path.as_deref()) {
        unsupported.push(format!(
            "OMP settings authority {}",
            redact_path_for_status(path, home.as_deref())
        ));
    }
    if let Some(drift) = omp_user_config_drift.filter(|drift| drift.unsupported) {
        unsupported.push(format!(
            "active OMP user config {}",
            redact_path_for_status(&drift.path, home.as_deref())
        ));
    }
    if let Some(drift) = omp_mcp_alias_drift {
        unsupported.extend(drift.unsupported_paths.iter().map(|path| {
            format!(
                "OMP MCP discovery authority {}",
                redact_path_for_status(path, home.as_deref())
            )
        }));
    }
    if !unsupported.is_empty() {
        return format!(
            "inspect unsupported {}, then {dry_run}; {fix}",
            unsupported.join(" and ")
        );
    }

    let mut authorities = Vec::new();
    if let Some(drift) = omp_settings_drift
        && !drift.project_config_enabled
    {
        let source = drift.effective_source.as_deref().map_or_else(
            || "OMP's effective settings".to_string(),
            |path| redact_path_for_status(path, home.as_deref()),
        );
        authorities.push(format!(
            "effective OMP setting mcp.enableProjectConfig=false from {source} excludes every project MCP config"
        ));
    }
    if let Some(drift) = omp_user_config_drift.filter(|drift| drift.disabled) {
        authorities.push(format!(
            "active OMP user config {} globally disables Agent Mail",
            redact_path_for_status(&drift.path, home.as_deref())
        ));
    }
    if let Some(drift) = omp_mcp_alias_drift.filter(|drift| !drift.conflict_paths.is_empty()) {
        let paths = drift
            .conflict_paths
            .iter()
            .map(|path| redact_path_for_status(path, home.as_deref()))
            .collect::<Vec<_>>()
            .join(", ");
        authorities.push(format!(
            "OMP loads a distinct noncanonical Agent Mail server alias from {paths}; hand-edit that read-only source to keep only `mcp-agent-mail`"
        ));
    }
    format!("{}; {dry_run}; {fix}", authorities.join("; "))
}

fn urls_match_for_status(actual_url: &str, expected_url: &str) -> bool {
    if actual_url == expected_url {
        return true;
    }
    let Some(actual) = parse_http_url_for_status(actual_url) else {
        return false;
    };
    let Some(expected) = parse_http_url_for_status(expected_url) else {
        return false;
    };
    actual.scheme == expected.scheme
        && status_url_hosts_match(&actual.host, &expected.host)
        && actual.port == expected.port
        && actual.path == expected.path
}

fn status_url_hosts_match(actual_host: &str, expected_host: &str) -> bool {
    if actual_host.eq_ignore_ascii_case(expected_host) {
        return true;
    }
    if actual_host.eq_ignore_ascii_case("localhost") {
        return is_status_loopback_host(expected_host);
    }
    if expected_host.eq_ignore_ascii_case("localhost") {
        return is_status_loopback_host(actual_host);
    }
    false
}

fn is_status_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusUrlParts {
    scheme: &'static str,
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url_for_status(url: &str) -> Option<StatusUrlParts> {
    let trimmed = url.trim();
    let (scheme, remainder, default_port) = if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest, 80_u16)
    } else {
        let rest = trimmed.strip_prefix("https://")?;
        ("https", rest, 443_u16)
    };

    let (authority, raw_path) = if let Some((auth, tail)) = remainder.split_once('/') {
        (auth, format!("/{tail}"))
    } else {
        (remainder, "/".to_string())
    };

    let (host, port) = parse_status_url_authority(authority, default_port)?;
    let path = normalize_status_url_path(&raw_path);

    Some(StatusUrlParts {
        scheme,
        host,
        port,
        path,
    })
}

fn parse_status_url_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.is_empty() {
        return None;
    }

    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = if tail.is_empty() {
            default_port
        } else {
            let port_str = tail.strip_prefix(':')?;
            port_str.parse::<u16>().ok()?
        };
        return Some((host.to_string(), port));
    }

    if authority.matches(':').count() == 1
        && let Some((host, port_str)) = authority.rsplit_once(':')
    {
        let port = port_str.parse::<u16>().ok()?;
        return Some((host.to_string(), port));
    }

    Some((authority.to_string(), default_port))
}

fn normalize_status_url_path(path: &str) -> String {
    let truncated = path.split(['?', '#']).next().unwrap_or(path).trim();
    let mut normalized = if truncated.is_empty() {
        "/".to_string()
    } else if truncated.starts_with('/') {
        truncated.to_string()
    } else {
        format!("/{truncated}")
    };
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    fn setup_real_tempdir() -> tempfile::TempDir {
        let temp_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        tempfile::Builder::new()
            .prefix("mcp-agent-mail-setup-")
            .tempdir_in(temp_root)
            .expect("setup temp directory")
    }

    enum EnvVarPrevious {
        Missing,
        Present(Option<String>),
    }

    struct EnvVarGuard {
        key: String,
        previous: EnvVarPrevious,
    }

    struct RandomFailureGuard {
        previous: bool,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: impl Into<String>) -> Self {
            let previous = TEST_ENV_OVERRIDES.with(|cell| {
                let mut map = cell.borrow_mut();
                let previous = map
                    .get(key)
                    .cloned()
                    .map_or(EnvVarPrevious::Missing, EnvVarPrevious::Present);
                map.insert(key.to_string(), Some(value.into()));
                previous
            });
            Self {
                key: key.to_string(),
                previous,
            }
        }

        fn unset(key: &str) -> Self {
            let previous = TEST_ENV_OVERRIDES.with(|cell| {
                let mut map = cell.borrow_mut();
                let previous = map
                    .get(key)
                    .cloned()
                    .map_or(EnvVarPrevious::Missing, EnvVarPrevious::Present);
                map.insert(key.to_string(), None);
                previous
            });
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl RandomFailureGuard {
        fn enable() -> Self {
            let previous = TEST_RANDOM_FAILURE.with(|cell| cell.replace(true));
            Self { previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            TEST_ENV_OVERRIDES.with(|cell| {
                let mut map = cell.borrow_mut();
                match &self.previous {
                    EnvVarPrevious::Present(previous) => {
                        map.insert(self.key.clone(), previous.clone());
                    }
                    EnvVarPrevious::Missing => {
                        map.remove(&self.key);
                    }
                }
            });
        }
    }

    impl Drop for RandomFailureGuard {
        fn drop(&mut self) {
            TEST_RANDOM_FAILURE.with(|cell| cell.set(self.previous));
        }
    }

    #[test]
    fn generate_token_is_64_hex_chars() {
        let t = generate_token().expect("token generation should succeed");
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_unique_across_calls() {
        let t1 = generate_token().expect("first token generation should succeed");
        let t2 = generate_token().expect("second token generation should succeed");
        assert_ne!(t1, t2);
    }

    #[test]
    fn generate_token_reports_rng_failure() {
        let _guard = RandomFailureGuard::enable();
        let error = generate_token().expect_err("rng failure should surface");
        assert!(error.to_string().contains("CSPRNG failure"));
    }

    #[test]
    fn resolve_token_explicit_wins() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let t = resolve_token(Some("my-explicit-token"), tmp.path())
            .expect("explicit token should resolve");
        assert_eq!(t, "my-explicit-token");
    }

    #[test]
    fn resolve_token_generates_when_no_source() {
        let _env = EnvVarGuard::unset("HTTP_BEARER_TOKEN");
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-env");
        let t = resolve_token(None, &missing).expect("token should be generated");
        assert_eq!(t.len(), 64);
    }

    #[test]
    fn resolve_token_reads_env_file() {
        let _env = EnvVarGuard::unset("HTTP_BEARER_TOKEN");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "HTTP_BEARER_TOKEN=\"double-quoted-token\"\n").unwrap();
        let t = resolve_token(None, tmp.path()).expect("env file token should resolve");
        assert_eq!(t, "double-quoted-token");
    }

    #[test]
    fn resolve_token_env_file_single_quoted() {
        let _env = EnvVarGuard::unset("HTTP_BEARER_TOKEN");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "HTTP_BEARER_TOKEN='single-quoted-token'\n").unwrap();
        let t = resolve_token(None, tmp.path()).expect("env file token should resolve");
        assert_eq!(t, "single-quoted-token");
    }

    #[test]
    fn resolve_token_empty_explicit_falls_through() {
        let _env = EnvVarGuard::unset("HTTP_BEARER_TOKEN");
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-env");
        let t = resolve_token(Some(""), &missing).expect("token should be generated");
        // Empty explicit should not be used; should fall through to generate
        assert_eq!(t.len(), 64);
    }

    #[test]
    fn resolve_token_propagates_rng_failure_when_generation_needed() {
        let _env = EnvVarGuard::unset("HTTP_BEARER_TOKEN");
        let _guard = RandomFailureGuard::enable();
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-env");
        let error = resolve_token(None, &missing).expect_err("rng failure should surface");
        assert!(error.to_string().contains("CSPRNG failure"));
    }

    #[test]
    fn merge_mcp_server_empty() {
        let result = merge_mcp_server(
            None,
            "mcpServers",
            "test-server",
            json!({"url": "http://localhost"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["mcpServers"]["test-server"]["url"], "http://localhost");
    }

    #[test]
    fn merge_mcp_server_existing_preserves_others() {
        let existing = r#"{"mcpServers": {"other-server": {"url": "http://other"}}}"#;
        let result = merge_mcp_server(
            Some(existing),
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://new"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["mcpServers"]["other-server"]["url"], "http://other");
        assert_eq!(doc["mcpServers"]["mcp-agent-mail"]["url"], "http://new");
    }

    #[test]
    fn merge_mcp_server_updates_stale_entry() {
        let existing = r#"{"mcpServers": {"mcp-agent-mail": {"url": "http://old"}}}"#;
        let result = merge_mcp_server(
            Some(existing),
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://new"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["mcpServers"]["mcp-agent-mail"]["url"], "http://new");
    }

    #[test]
    fn merge_mcp_server_rewrites_underscore_alias_without_duplicate() {
        let existing = r#"{"mcpServers": {"mcp_agent_mail": {"url": "http://old"}, "other": {"url": "http://other"}}}"#;
        let result = merge_mcp_server(
            Some(existing),
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://new"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        let servers = doc["mcpServers"].as_object().expect("servers object");
        assert_eq!(servers["mcp-agent-mail"]["url"], "http://new");
        assert!(
            !servers.contains_key("mcp_agent_mail"),
            "legacy underscore alias should be removed"
        );
        assert_eq!(servers["other"]["url"], "http://other");
    }

    #[test]
    fn merge_mcp_server_preserves_other_keys() {
        let existing = r#"{"someOtherSetting": true, "mcpServers": {}}"#;
        let result = merge_mcp_server(
            Some(existing),
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://localhost"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["someOtherSetting"], json!(true));
    }

    #[test]
    fn merge_claude_local_scope_mcp_creates_nested_path() {
        // GH#168: from empty, builds projects.<path>.mcpServers.<name>.
        let result = merge_claude_local_scope_mcp(
            None,
            "/abs/repo",
            "mcp-agent-mail",
            json!({"type": "http", "url": "http://127.0.0.1:8765/mcp/"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            doc["projects"]["/abs/repo"]["mcpServers"]["mcp-agent-mail"]["url"],
            json!("http://127.0.0.1:8765/mcp/")
        );
    }

    #[test]
    fn merge_claude_local_scope_mcp_preserves_unrelated_keys() {
        // Unrelated top-level keys, other projects, and the user-scope top-level
        // mcpServers must all survive the merge.
        let existing = r#"{
  "numStartups": 7,
  "mcpServers": { "other-user-server": { "url": "x" } },
  "projects": {
    "/other/repo": { "mcpServers": { "keep": { "url": "y" } } },
    "/abs/repo": { "allowedTools": ["Bash"] }
  }
}"#;
        let result = merge_claude_local_scope_mcp(
            Some(existing),
            "/abs/repo",
            "mcp-agent-mail",
            json!({"url": "z"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["numStartups"], json!(7));
        assert_eq!(doc["mcpServers"]["other-user-server"]["url"], json!("x"));
        assert_eq!(
            doc["projects"]["/other/repo"]["mcpServers"]["keep"]["url"],
            json!("y")
        );
        // existing per-project keys preserved alongside the inserted server
        assert_eq!(
            doc["projects"]["/abs/repo"]["allowedTools"],
            json!(["Bash"])
        );
        assert_eq!(
            doc["projects"]["/abs/repo"]["mcpServers"]["mcp-agent-mail"]["url"],
            json!("z")
        );
    }

    #[test]
    fn merge_claude_local_scope_mcp_idempotent_and_dedupes_alias() {
        // Re-running replaces in place; the underscore alias is removed.
        let seeded = r#"{"projects":{"/r":{"mcpServers":{"mcp_agent_mail":{"url":"old"}}}}}"#;
        let result = merge_claude_local_scope_mcp(
            Some(seeded),
            "/r",
            "mcp-agent-mail",
            json!({"url": "new"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        let servers = doc["projects"]["/r"]["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("mcp_agent_mail"),
            "alias must be dropped"
        );
        assert_eq!(servers["mcp-agent-mail"]["url"], json!("new"));
        assert_eq!(servers.len(), 1);
    }

    #[test]
    fn config_actions_cursor() {
        let params = SetupParams {
            host: "127.0.0.1".into(),
            port: 8765,
            path: "/mcp/".into(),
            token: "test-token".into(),
            project_dir: PathBuf::from("/tmp/project"),
            skip_user_config: true,
            ..Default::default()
        };
        let actions = AgentPlatform::Cursor.config_actions(&params);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].file_path.ends_with("cursor.mcp.json"));
        match &actions[0].content {
            ConfigContent::JsonMerge {
                servers_key,
                server_value,
                ..
            } => {
                assert_eq!(*servers_key, "mcpServers");
                assert_eq!(server_value["type"], "http");
                assert!(server_value["url"].as_str().unwrap().contains("8765"));
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    // ---- security issue #148: bearer token in *.mcp.json must be gitignored ----

    /// Every project-local config file a platform writes that can embed a token
    /// must be covered by the gitignore `run_setup` generates.
    #[test]
    fn run_setup_gitignore_covers_every_emitted_token_bearing_file() {
        let tmp = setup_real_tempdir();
        let params = SetupParams {
            host: "127.0.0.1".into(),
            port: 8765,
            path: "/mcp/".into(),
            token: "live-secret-token".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(AgentPlatform::ALL.to_vec()),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };
        let _ = run_setup(&params);

        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        let lines: Vec<&str> = gitignore.lines().map(str::trim).collect();
        assert!(
            lines.contains(&".env"),
            "gitignore must cover .env: {gitignore}"
        );

        // For every platform, every project-local token-bearing file it writes
        // must appear in the generated .gitignore.
        for platform in AgentPlatform::ALL {
            for file in platform.project_local_secret_files() {
                assert!(
                    lines.contains(file),
                    "gitignore is missing {file} (platform {platform}); contents:\n{gitignore}"
                );
            }
        }
        // Spot-check the high-risk filenames the old hardcoded list missed.
        // (GH#168: Claude no longer writes a token-bearing file into the project
        // dir — its MCP config lives in `~/.claude.json` — so it is no longer
        // expected here.)
        for expected in [
            "cursor.mcp.json",
            "gemini.mcp.json",
            ".omp/mcp.json",
            "factory.mcp.json",
            "windsurf.mcp.json",
            "cline.mcp.json",
            "opencode.json",
            ".vscode/mcp.json",
        ] {
            assert!(
                lines.contains(&expected),
                "gitignore must cover {expected}: {gitignore}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_setup_refuses_project_secret_when_gitignore_cannot_be_secured() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside-gitignore");
        std::fs::write(&outside, "sentinel\n").unwrap();
        symlink(&outside, tmp.path().join(".gitignore")).unwrap();
        let params = SetupParams {
            token: "live-secret-token".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        assert!(matches!(
            results[0].actions[0].outcome,
            ActionOutcome::Failed(_)
        ));
        assert!(!tmp.path().join(".omp/mcp.json").exists());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel\n");
    }

    #[test]
    fn run_setup_refuses_literal_bearer_in_git_tracked_omp_config() {
        let tmp = setup_real_tempdir();
        let config = tmp.path().join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "{\"mcpServers\":{\"operator-owned\":{\"command\":\"keep\"}}}\n";
        std::fs::write(&config, original).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(["add", "--", ".omp/mcp.json"])
                .status()
                .unwrap()
                .success()
        );
        let params = SetupParams {
            token: "must-not-enter-index".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        let ActionOutcome::Failed(error) = &results[0].actions[0].outcome else {
            panic!("tracked OMP config must fail closed: {results:?}");
        };
        assert!(error.contains("Git-tracked config"), "{error}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(
            std::fs::read_dir(config.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".bak"))
        );
    }

    #[test]
    fn run_setup_dry_run_refuses_tracked_omp_config_without_writing() {
        let tmp = setup_real_tempdir();
        let config = tmp.path().join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "{\"mcpServers\":{\"operator-owned\":{\"command\":\"keep\"}}}\n";
        std::fs::write(&config, original).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(["add", "--", ".omp/mcp.json"])
                .status()
                .unwrap()
                .success()
        );
        let flock_sentinel = tmp.path().join(".git/am.git-serialize.lock");
        assert!(!flock_sentinel.exists());
        let params = SetupParams {
            token: "must-not-enter-index".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            dry_run: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        let ActionOutcome::Failed(error) = &results[0].actions[0].outcome else {
            panic!("tracked OMP config must fail closed in dry-run: {results:?}");
        };
        assert!(error.contains("Git-tracked config"), "{error}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(!tmp.path().join(".gitignore").exists());
        assert!(!flock_sentinel.exists());
    }

    #[cfg(unix)]
    #[test]
    fn run_setup_dry_run_refuses_symlinked_omp_config_without_writing() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let config = tmp.path().join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let outside = tmp.path().join("outside-config.json");
        let original = "{\"operator\":\"owned\"}\n";
        std::fs::write(&outside, original).unwrap();
        symlink(&outside, &config).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let flock_sentinel = tmp.path().join(".git/am.git-serialize.lock");
        let params = SetupParams {
            token: "must-not-reach-symlink".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            dry_run: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        let ActionOutcome::Failed(error) = &results[0].actions[0].outcome else {
            panic!("symlinked OMP config must fail closed in dry-run: {results:?}");
        };
        assert!(error.contains("must not be a symlink"), "{error}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), original);
        assert!(!tmp.path().join(".gitignore").exists());
        assert!(!flock_sentinel.exists());
    }

    #[cfg(unix)]
    #[test]
    fn run_setup_dry_run_refuses_symlinked_gitignore_without_writing() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let config = tmp.path().join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original_config = "{\"operator\":\"owned\"}\n";
        std::fs::write(&config, original_config).unwrap();
        let outside = tmp.path().join("outside-gitignore");
        let original_gitignore = "operator-owned\n";
        std::fs::write(&outside, original_gitignore).unwrap();
        symlink(&outside, tmp.path().join(".gitignore")).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let flock_sentinel = tmp.path().join(".git/am.git-serialize.lock");
        let params = SetupParams {
            token: "must-not-reach-unsafe-gitignore".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            dry_run: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        let ActionOutcome::Failed(error) = &results[0].actions[0].outcome else {
            panic!("symlinked .gitignore must fail closed in dry-run: {results:?}");
        };
        assert!(error.contains("must not be a symlink"), "{error}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original_config);
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            original_gitignore
        );
        assert!(!flock_sentinel.exists());
    }

    #[test]
    fn run_setup_token_rotation_keeps_live_config_and_backup_out_of_git_status() {
        let tmp = setup_real_tempdir();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let mut params = SetupParams {
            token: "old-secret-token".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };
        let first = run_setup(&params);
        assert!(matches!(
            first[0].actions[0].outcome,
            ActionOutcome::Created
        ));
        params.token = "new-secret-token".into();
        let second = run_setup(&params);
        assert!(matches!(
            second[0].actions[0].outcome,
            ActionOutcome::Updated
        ));

        let backup = std::fs::read_dir(tmp.path().join(".omp"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .expect("token rotation must preserve the old config in a backup");
        assert!(
            std::fs::read_to_string(backup.path())
                .unwrap()
                .contains("Bearer old-secret-token")
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status = String::from_utf8(status.stdout).unwrap();
        assert!(
            !status.contains(".omp/"),
            "secret artifacts leaked: {status}"
        );
        assert!(!status.contains(".bak"), "backup leaked: {status}");
    }

    #[test]
    fn run_setup_token_removal_protects_backup_of_old_literal_secret() {
        let tmp = setup_real_tempdir();
        let config = tmp.path().join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","enabled":true,"headers":{"Authorization":"Bearer old-secret-token"}}}}"#,
        )
        .unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let params = SetupParams {
            token: String::new(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };

        let result = run_setup(&params);
        assert!(matches!(
            result[0].actions[0].outcome,
            ActionOutcome::Updated
        ));
        assert!(
            !std::fs::read_to_string(&config)
                .unwrap()
                .contains("old-secret-token")
        );
        let backup = std::fs::read_dir(config.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .expect("old literal secret must be backed up before removal");
        assert!(
            std::fs::read_to_string(backup.path())
                .unwrap()
                .contains("Bearer old-secret-token")
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status = String::from_utf8(status.stdout).unwrap();
        assert!(
            !status.contains(".omp/"),
            "secret artifacts leaked: {status}"
        );
        assert!(!status.contains(".bak"), "backup leaked: {status}");
    }

    #[test]
    fn run_setup_gitignores_project_local_omp_agent_dir_override() {
        let tmp = setup_real_tempdir();
        let override_path = tmp.path().join("custom-agent/mcp.json");
        let params = SetupParams {
            token: "live-secret-token".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            omp_user_config_path_override: Some(override_path.clone()),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: false,
            skip_hooks: true,
            ..Default::default()
        };

        let results = run_setup(&params);
        assert!(
            results[0]
                .actions
                .iter()
                .all(|action| !matches!(action.outcome, ActionOutcome::Failed(_)))
        );
        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(
            gitignore
                .lines()
                .any(|line| line == "/custom-agent/mcp.json")
        );
        assert!(
            std::fs::read_to_string(override_path)
                .unwrap()
                .contains("Bearer live-secret-token")
        );
    }

    #[test]
    fn run_setup_escapes_gitignore_metacharacters_in_omp_override_path() {
        let tmp = setup_real_tempdir();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let override_path = tmp.path().join("custom[agent]/mcp*.json");
        let mut params = SetupParams {
            token: "first-secret".into(),
            project_dir: tmp.path().to_path_buf(),
            home_dir_override: Some(tmp.path().join("home")),
            omp_user_config_path_override: Some(override_path.clone()),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: false,
            skip_hooks: true,
            ..Default::default()
        };
        assert!(
            run_setup(&params)[0]
                .actions
                .iter()
                .all(|action| !matches!(action.outcome, ActionOutcome::Failed(_)))
        );
        params.token = "second-secret".into();
        assert!(
            run_setup(&params)[0]
                .actions
                .iter()
                .all(|action| !matches!(action.outcome, ActionOutcome::Failed(_)))
        );

        let status = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status = String::from_utf8(status.stdout).unwrap();
        assert!(
            !status.contains("custom[agent]"),
            "metacharacter path was not ignored literally: {status}"
        );
        assert!(
            std::fs::read_dir(override_path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".bak")),
            "token rotation should exercise the escaped backup pattern"
        );
    }

    /// `project_local_secret_files()` must list exactly the project-dir files
    /// each platform's `config_actions` actually writes (keep them in sync so a
    /// new client doesn't leak a token). User-level (home) files are excluded.
    #[test]
    fn project_local_secret_files_matches_emitted_project_dir_actions() {
        // home_dir MUST be outside project_dir, otherwise user-level configs
        // (e.g. ~/.claude/settings.json) would appear nested under the project.
        let proj_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let pdir = proj_tmp.path();
        let home = home_tmp.path().to_path_buf();
        for platform in AgentPlatform::ALL {
            let params = SetupParams {
                token: "tok".into(),
                project_dir: pdir.to_path_buf(),
                home_dir_override: Some(home.clone()),
                agents: Some(vec![*platform]),
                skip_user_config: false,
                skip_hooks: false,
                ..Default::default()
            };
            // Project-dir-relative file paths this platform writes.
            let emitted: Vec<String> = platform
                .config_actions(&params)
                .iter()
                .filter(|a| a.file_path.starts_with(pdir))
                .filter_map(|a| {
                    a.file_path
                        .strip_prefix(pdir)
                        .ok()
                        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                })
                // Claude's hooks file (.claude/settings.json) is project-local
                // but carries no token; the secret file is settings.local.json.
                .filter(|rel| rel != ".claude/settings.json")
                .collect();
            let declared: Vec<String> = platform
                .project_local_secret_files()
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            for rel in &emitted {
                assert!(
                    declared.contains(rel),
                    "{platform}: project-local file {rel} is written but not declared in \
                     project_local_secret_files() (would leak under git add -A)"
                );
            }
        }
    }

    /// Under `--no-auth` (empty token), no `Authorization` header is written
    /// into any project-local config — never a live or blank bearer credential.
    #[test]
    fn empty_token_writes_no_authorization_header() {
        for platform in AgentPlatform::ALL {
            let params = SetupParams {
                token: String::new(),
                project_dir: PathBuf::from("/tmp/p"),
                home_dir_override: Some(PathBuf::from("/tmp/home")),
                skip_user_config: true,
                skip_hooks: true,
                ..Default::default()
            };
            for action in platform.config_actions(&params) {
                let serialized = match &action.content {
                    ConfigContent::JsonMerge { server_value, .. }
                    | ConfigContent::ClaudeLocalScopeMcp { server_value, .. } => {
                        serde_json::to_string(server_value).unwrap()
                    }
                    ConfigContent::JsonFull(v) => serde_json::to_string(v).unwrap(),
                    ConfigContent::TomlSection { key_values, .. } => key_values
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    ConfigContent::HooksMerge { .. } => String::new(),
                };
                assert!(
                    !serialized.contains("Bearer") && !serialized.contains("Authorization"),
                    "{platform} emitted an Authorization header with an empty token: {serialized}"
                );
            }
        }
    }

    /// With a real token, the Authorization header IS present (regression guard
    /// so the empty-token suppression doesn't strip auth from authed runs).
    #[test]
    fn nonempty_token_writes_authorization_header() {
        let params = SetupParams {
            token: "real-token".into(),
            project_dir: PathBuf::from("/tmp/p"),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };
        let actions = AgentPlatform::Cursor.config_actions(&params);
        let ConfigContent::JsonMerge {
            server_value: value,
            ..
        } = &actions[0].content
        else {
            panic!("expected JsonMerge");
        };
        assert_eq!(value["headers"]["Authorization"], "Bearer real-token");
    }

    #[test]
    fn config_actions_gemini_uses_http_url() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            skip_user_config: true,
            ..Default::default()
        };
        let actions = AgentPlatform::Gemini.config_actions(&params);
        assert_eq!(actions.len(), 1);
        match &actions[0].content {
            ConfigContent::JsonMerge { server_value, .. } => {
                assert!(server_value.get("httpUrl").is_some(), "Gemini uses httpUrl");
                assert!(
                    server_value.get("type").is_none(),
                    "Gemini has no type field"
                );
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    #[test]
    fn config_actions_omp_uses_native_http_config_paths() {
        let home = PathBuf::from("/tmp/omp-home");
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            home_dir_override: Some(home.clone()),
            skip_user_config: false,
            ..Default::default()
        };
        let actions = AgentPlatform::Omp.config_actions(&params);
        assert_eq!(
            actions.len(),
            2,
            "project-local + default-profile user config"
        );
        assert_eq!(actions[0].file_path, PathBuf::from("/tmp/p/.omp/mcp.json"));
        assert_eq!(actions[1].file_path, home.join(".omp/agent/mcp.json"));

        for action in &actions {
            match &action.content {
                ConfigContent::JsonMerge {
                    servers_key,
                    server_value,
                    ..
                } => {
                    assert_eq!(*servers_key, "mcpServers");
                    assert_eq!(server_value["type"], "http");
                    assert_eq!(server_value["url"], "http://127.0.0.1:8765/mcp/");
                    assert_eq!(server_value["headers"]["Authorization"], "Bearer tok");
                    assert_eq!(server_value["enabled"], true);
                }
                _ => panic!("expected OMP JsonMerge action"),
            }
        }
    }

    #[test]
    fn omp_setup_reenables_project_entry_removes_stale_oauth_and_preserves_project_lists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // macOS exposes its default temporary directory through the `/var`
        // compatibility symlink, which the production writer intentionally
        // refuses to traverse. Exercise the writer through the real path.
        let tmp_root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let project_dir = tmp_root.join("project");
        let config_path = project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("create config parent");
        std::fs::write(
            &config_path,
            r#"{
  "disabledServers": ["other", "mcp_agent_mail", "mcp-agent-mail", "agent-mail"],
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/api/",
      "enabled": "false",
      "timeout": 45000,
      "requestIdFormat": "string",
      "auth": {
        "type": "oauth",
        "credentialId": "stale-credential"
      },
      "oauth": {
        "clientId": "stale-client",
        "authorizationUrl": "https://stale.example/authorize"
      },
      "headers": {
        "authorization": "Bearer stale",
        "X-Trace": "preserve-me"
      }
    },
    "sibling": {"command": "node"}
  },
  "servers": {
    "agent-mail": {"command": "legacy-agent-mail", "args": []},
    "legacy-sibling": {"command": "node"}
  }
}
"#,
        )
        .expect("write disabled OMP config");

        let params = SetupParams {
            token: "tok".into(),
            project_dir,
            home_dir_override: Some(tmp_root.join("home")),
            skip_user_config: true,
            ..Default::default()
        };
        let action = AgentPlatform::Omp
            .config_actions(&params)
            .into_iter()
            .next()
            .expect("OMP project action");
        assert_eq!(
            write_config_atomic(&action, true).expect("write config"),
            ActionOutcome::Updated
        );

        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(&config_path).expect("read updated config"),
        )
        .expect("parse updated config");
        assert_eq!(
            doc["mcpServers"]["mcp-agent-mail"]["enabled"],
            Value::Bool(true)
        );
        assert_eq!(doc["mcpServers"]["mcp-agent-mail"]["timeout"], 45000);
        assert_eq!(
            doc["mcpServers"]["mcp-agent-mail"]["requestIdFormat"],
            "string"
        );
        assert!(
            doc["mcpServers"]["mcp-agent-mail"].get("auth").is_none(),
            "an explicit OAuth credential can replace the setup bearer at runtime"
        );
        assert!(
            doc["mcpServers"]["mcp-agent-mail"].get("oauth").is_none(),
            "stale OAuth client metadata must not survive canonical bearer setup"
        );
        assert_eq!(
            doc["mcpServers"]["mcp-agent-mail"]["headers"],
            json!({"Authorization": "Bearer tok", "X-Trace": "preserve-me"})
        );
        assert_eq!(doc["mcpServers"]["sibling"]["command"], "node");
        assert!(doc["servers"].get("agent-mail").is_none());
        assert_eq!(doc["servers"]["legacy-sibling"]["command"], "node");
        assert_eq!(
            doc["disabledServers"],
            json!(["other", "mcp_agent_mail", "mcp-agent-mail", "agent-mail"]),
            "OMP ignores project runtime lists, so setup must leave them untouched"
        );
        assert_eq!(
            write_config_atomic(&action, true).expect("idempotent rewrite"),
            ActionOutcome::Unchanged
        );
    }

    #[test]
    fn omp_setup_refuses_malformed_active_user_runtime_lists_without_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp_root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let project_dir = tmp_root.join("project");
        let home = tmp_root.join("home");
        let config_path = home.join(".omp/agent/mcp.json");
        std::fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("create config parent");
        let original = r#"{
  "disabledServers": ["mcp-agent-mail", 7],
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "enabled": false
    }
  }
}
"#;
        std::fs::write(&config_path, original).expect("write malformed OMP config");

        let params = SetupParams {
            token: "tok".into(),
            project_dir,
            home_dir_override: Some(home),
            skip_user_config: false,
            ..Default::default()
        };
        let action = AgentPlatform::Omp
            .config_actions(&params)
            .into_iter()
            .find(|action| action.file_path == config_path)
            .expect("OMP active-user action");
        let error = write_config_atomic(&action, true)
            .expect_err("malformed OMP disablement authority must fail closed");

        assert!(error.to_string().contains("entries must be strings"));
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read untouched config"),
            original
        );
    }

    #[test]
    fn omp_setup_reconciles_only_agent_mail_names_in_active_user_runtime_lists() {
        let tmp = setup_real_tempdir();
        let project_dir = tmp.path().join("project");
        let home = tmp.path().join("home");
        let config_path = home.join(".omp/agent/mcp.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            r#"{
  "disabledServers": ["sibling", "mcp-agent-mail"],
  "enabledServers": ["mcp_agent_mail", "agent-mail"],
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://stale.example/mcp",
      "enabled": false
    }
  }
}"#,
        )
        .unwrap();
        let params = SetupParams {
            token: "tok".into(),
            project_dir,
            home_dir_override: Some(home),
            skip_user_config: false,
            ..Default::default()
        };
        let action = AgentPlatform::Omp
            .config_actions(&params)
            .into_iter()
            .find(|action| action.file_path == config_path)
            .expect("OMP active-user action");

        write_config_atomic(&action, true).expect("reconcile active-user OMP config");

        let document: Value = serde_json::from_str(
            &std::fs::read_to_string(&config_path).expect("read active-user OMP config"),
        )
        .unwrap();
        assert_eq!(document["disabledServers"], json!(["sibling"]));
        assert_eq!(document["enabledServers"], json!([]));
        assert_eq!(document["mcpServers"]["mcp-agent-mail"]["enabled"], true);
    }

    #[test]
    fn config_actions_omp_honors_resolved_active_profile_path() {
        let active_profile_config =
            PathBuf::from("/tmp/omp-home/.omp/profiles/work/agent/mcp.json");
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            home_dir_override: Some(PathBuf::from("/tmp/omp-home")),
            omp_user_config_path_override: Some(active_profile_config.clone()),
            skip_user_config: false,
            ..Default::default()
        };

        let actions = AgentPlatform::Omp.config_actions(&params);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[1].file_path, active_profile_config);
        assert!(actions[1].description.contains("active-profile"));
    }

    #[test]
    fn omp_home_authority_must_be_absolute() {
        let missing =
            require_absolute_omp_home_dir(None).expect_err("missing OMP home must fail closed");
        assert!(
            missing
                .to_string()
                .contains("absolute, traversal-free home directory")
        );

        let relative = require_absolute_omp_home_dir(Some(PathBuf::from("home")))
            .expect_err("relative OMP home must fail closed");
        assert!(
            relative
                .to_string()
                .contains("absolute, traversal-free home directory")
        );

        let temp = setup_real_tempdir();
        let absolute = temp.path().join("home");
        assert_eq!(
            require_absolute_omp_home_dir(Some(absolute.clone())).unwrap(),
            absolute
        );

        let traversing = temp.path().join("home/../outside");
        let error = require_absolute_omp_home_dir(Some(traversing))
            .expect_err("an absolute path with parent traversal must fail closed");
        assert!(error.to_string().contains("traversal-free"));
    }

    #[test]
    fn run_setup_omp_fails_before_writes_without_absolute_user_authority() {
        let temp = setup_real_tempdir();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let sentinel = project.join("operator-owned");
        std::fs::write(&sentinel, "sentinel\n").unwrap();
        let _missing_process_home = EnvVarGuard::unset(TEST_OMP_HOME_DIR_OVERRIDE_KEY);

        for mut params in [
            SetupParams {
                project_dir: project.clone(),
                agents: Some(vec![AgentPlatform::Omp]),
                token: "must-not-be-written".to_string(),
                skip_hooks: true,
                ..SetupParams::default()
            },
            SetupParams {
                project_dir: project.clone(),
                home_dir_override: Some(PathBuf::from("relative-home")),
                agents: Some(vec![AgentPlatform::Omp]),
                token: "must-not-be-written".to_string(),
                skip_hooks: true,
                ..SetupParams::default()
            },
            SetupParams {
                project_dir: project.clone(),
                omp_user_config_path_override: Some(temp.path().join("user/../escaped/mcp.json")),
                agents: Some(vec![AgentPlatform::Omp]),
                token: "must-not-be-written".to_string(),
                skip_hooks: true,
                ..SetupParams::default()
            },
        ] {
            params.skip_user_config = true;
            let results = run_setup(&params);
            assert_eq!(results.len(), 1);
            let ActionOutcome::Failed(error) = &results[0].actions[0].outcome else {
                panic!("unresolved OMP authority must fail closed: {results:?}");
            };
            assert!(error.contains("before writing any config bytes"));
            assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "sentinel\n");
            assert!(!project.join(".omp").exists());
            assert!(!project.join(".gitignore").exists());
        }

        drop(_missing_process_home);
        let process_home = temp.path().join("process-home");
        let _absolute_process_home = EnvVarGuard::set(
            TEST_OMP_HOME_DIR_OVERRIDE_KEY,
            process_home.display().to_string(),
        );
        let params = SetupParams {
            project_dir: project.clone(),
            agents: Some(vec![AgentPlatform::Omp]),
            token: "ordinary-direct-setup".to_string(),
            skip_hooks: true,
            ..SetupParams::default()
        };
        let results = run_setup(&params);
        assert!(
            results[0]
                .actions
                .iter()
                .all(|action| !matches!(action.outcome, ActionOutcome::Failed(_)))
        );
        assert!(project.join(".omp/mcp.json").is_file());
        assert!(process_home.join(".omp/agent/mcp.json").is_file());
    }

    #[test]
    fn resolve_omp_config_paths_matches_v18_profile_precedence() {
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/repo");

        let named = resolve_omp_config_paths(
            home,
            cwd,
            Some(" work "),
            Some("legacy"),
            Some(".custom-omp"),
            Some("ignored-for-named-profile"),
        )
        .unwrap();
        assert_eq!(named.config_root, PathBuf::from("/home/alice/.custom-omp"));
        assert_eq!(
            named.user_mcp_config,
            PathBuf::from("/home/alice/.custom-omp/profiles/work/agent/mcp.json")
        );

        let explicit_default = resolve_omp_config_paths(
            home,
            cwd,
            Some(""),
            Some("legacy"),
            None,
            Some("relative-agent-dir"),
        )
        .unwrap();
        assert_eq!(
            explicit_default.user_mcp_config,
            PathBuf::from("/work/repo/relative-agent-dir/mcp.json"),
            "an explicitly empty OMP_PROFILE selects default and must not fall through to PI_PROFILE"
        );

        let legacy = resolve_omp_config_paths(
            home,
            cwd,
            None,
            Some("legacy"),
            None,
            Some("ignored-for-named-profile"),
        )
        .unwrap();
        assert_eq!(
            legacy.user_mcp_config,
            PathBuf::from("/home/alice/.omp/profiles/legacy/agent/mcp.json")
        );
    }

    #[test]
    fn resolve_omp_config_paths_rejects_traversal_and_ambiguous_prefixes() {
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/repo");

        for config_dir in [
            "../escape",
            ".omp/../../escape",
            "C:\\escape",
            "\\\\host\\share",
        ] {
            let error = resolve_omp_config_paths(home, cwd, None, None, Some(config_dir), None)
                .expect_err("unsafe PI_CONFIG_DIR must fail closed");
            assert!(error.to_string().contains("PI_CONFIG_DIR"));
        }

        for agent_dir in ["../escape", "agent/../../escape", "C:\\escape"] {
            let error = resolve_omp_config_paths(home, cwd, None, None, None, Some(agent_dir))
                .expect_err("unsafe PI_CODING_AGENT_DIR must fail closed");
            assert!(error.to_string().contains("PI_CODING_AGENT_DIR"));
        }

        let relative_cwd = resolve_omp_config_paths(
            home,
            Path::new("relative-cwd"),
            None,
            None,
            None,
            Some("agent"),
        )
        .expect_err("relative working directory must fail closed");
        assert!(relative_cwd.to_string().contains("working directory"));
    }

    #[test]
    fn resolve_omp_settings_overlays_matches_runtime_path_rules() {
        let joined = std::env::join_paths([
            Path::new("relative.yml"),
            Path::new("~/profile overlay.yml"),
            Path::new("/absolute/overlay.yml"),
        ])
        .unwrap();
        let paths = resolve_omp_settings_overlay_paths(
            Some(&joined),
            Path::new("/work/repo"),
            Some(Path::new("/home/alice")),
        )
        .unwrap();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/work/repo/relative.yml"),
                PathBuf::from("/home/alice/profile overlay.yml"),
                PathBuf::from("/absolute/overlay.yml"),
            ]
        );

        let tilde = std::env::join_paths([Path::new("~/overlay.yml")]).unwrap();
        let error = resolve_omp_settings_overlay_paths(Some(&tilde), Path::new("/work/repo"), None)
            .expect_err("a tilde overlay without a home directory must fail closed");
        assert!(error.to_string().contains("home directory is unavailable"));
    }

    #[test]
    fn resolve_omp_config_paths_rejects_invalid_profiles_like_runtime_boot() {
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/repo");
        for invalid in [".", "..", "bad profile", "Work", "CON", "LPT9.txt", "bad."] {
            let error = resolve_omp_config_paths(home, cwd, Some(invalid), None, None, None)
                .expect_err("invalid explicit profile must fail closed");
            assert!(
                error.to_string().contains("invalid OMP profile"),
                "unexpected error for {invalid:?}: {error}"
            );

            let legacy_error = resolve_omp_config_paths(home, cwd, None, Some(invalid), None, None)
                .expect_err("invalid legacy PI_PROFILE must also fail closed");
            assert!(
                legacy_error.to_string().contains("invalid OMP profile"),
                "unexpected PI_PROFILE error for {invalid:?}: {legacy_error}"
            );
        }

        let precedence_error =
            resolve_omp_config_paths(home, cwd, Some("Work"), Some("valid"), None, None)
                .expect_err("invalid OMP_PROFILE must not fall through to valid PI_PROFILE");
        assert!(precedence_error.to_string().contains("invalid OMP profile"));

        let default = resolve_omp_config_paths(home, cwd, Some("default"), None, None, None)
            .expect("the explicit default profile is valid");
        assert_eq!(
            default.user_mcp_config,
            PathBuf::from("/home/alice/.omp/agent/mcp.json")
        );
    }

    #[test]
    fn config_actions_antigravity_uses_http_url_and_gemini_config_path() {
        // bd-47kjh.7.2: agy reads ~/.gemini/config/mcp_config.json (verified by
        // stracing the live agy 1.0.7 binary), NOT ~/.gemini/settings.json.
        let home = PathBuf::from("/tmp/agyhome");
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            home_dir_override: Some(home.clone()),
            skip_user_config: false,
            ..Default::default()
        };
        let actions = AgentPlatform::Antigravity.config_actions(&params);
        assert_eq!(actions.len(), 2, "project-local + user-level");

        // Project-local agy.mcp.json carries httpUrl + the bearer header.
        let project = &actions[0];
        assert_eq!(project.file_path, PathBuf::from("/tmp/p/agy.mcp.json"));
        match &project.content {
            ConfigContent::JsonMerge {
                servers_key,
                server_value,
                ..
            } => {
                assert_eq!(*servers_key, "mcpServers");
                assert!(
                    server_value.get("httpUrl").is_some(),
                    "agy uses httpUrl (gemini-compatible schema)"
                );
                assert!(
                    server_value.get("type").is_none(),
                    "agy entry has no `type` field"
                );
                let auth = server_value
                    .get("headers")
                    .and_then(|h| h.get("Authorization"))
                    .and_then(Value::as_str);
                assert_eq!(auth, Some("Bearer tok"));
            }
            _ => panic!("expected JsonMerge for agy project-local config"),
        }

        // User-level config is ~/.gemini/config/mcp_config.json with NO token.
        let user = &actions[1];
        assert_eq!(
            user.file_path,
            home.join(".gemini").join("config").join("mcp_config.json"),
            "agy user-level config must live at ~/.gemini/config/mcp_config.json"
        );
        match &user.content {
            ConfigContent::JsonMerge { server_value, .. } => {
                assert!(server_value.get("httpUrl").is_some());
                assert!(
                    server_value.get("headers").is_none(),
                    "user-level agy config must NOT embed a bearer token (#148)"
                );
            }
            _ => panic!("expected JsonMerge for agy user-level config"),
        }
    }

    #[test]
    fn config_actions_antigravity_writes_no_token_under_no_auth() {
        // #148: empty token (am serve-http --no-auth) => no Authorization header
        // written into the project-local agy.mcp.json.
        let params = SetupParams {
            token: String::new(),
            project_dir: PathBuf::from("/tmp/p"),
            home_dir_override: Some(PathBuf::from("/tmp/home")),
            skip_user_config: true,
            ..Default::default()
        };
        let actions = AgentPlatform::Antigravity.config_actions(&params);
        assert_eq!(actions.len(), 1);
        match &actions[0].content {
            ConfigContent::JsonMerge { server_value, .. } => {
                let headers = server_value.get("headers").expect("headers object");
                assert!(
                    headers.get("Authorization").is_none(),
                    "no Authorization header may be written with an empty token"
                );
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    #[test]
    fn config_actions_codex_uses_http_url() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            path: "/api/".into(),
            ..Default::default()
        };
        let actions = AgentPlatform::Codex.config_actions(&params);
        assert_eq!(actions.len(), 1);
        match &actions[0].content {
            ConfigContent::TomlSection {
                section_header,
                key_values,
            } => {
                assert_eq!(section_header, "[mcp_servers.mcp_agent_mail]");
                assert!(
                    key_values.contains(&("url".into(), "\"http://127.0.0.1:8765/api/\"".into()))
                );
                assert!(key_values.contains(&("startup_timeout_sec".into(), "30".into())));
                assert!(key_values.contains(&(
                    "http_headers".into(),
                    "{ Authorization = \"Bearer tok\" }".into(),
                )));
            }
            _ => panic!("expected TomlSection"),
        }
    }

    #[test]
    fn config_actions_opencode_uses_mcp_key_and_remote_type() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            ..Default::default()
        };
        let actions = AgentPlatform::OpenCode.config_actions(&params);
        assert_eq!(actions.len(), 1);
        match &actions[0].content {
            ConfigContent::JsonMerge {
                servers_key,
                server_value,
                ..
            } => {
                assert_eq!(*servers_key, "mcp");
                assert_eq!(server_value["type"], "remote");
                assert_eq!(server_value["enabled"], true);
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    #[test]
    fn config_actions_copilot_uses_servers_key() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            ..Default::default()
        };
        let actions = AgentPlatform::GithubCopilot.config_actions(&params);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].file_path.ends_with(".vscode/mcp.json"));
        match &actions[0].content {
            ConfigContent::JsonMerge { servers_key, .. } => {
                assert_eq!(*servers_key, "servers");
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    #[test]
    fn config_actions_factory_no_type_field() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            skip_user_config: true,
            ..Default::default()
        };
        let actions = AgentPlatform::FactoryDroid.config_actions(&params);
        assert_eq!(actions.len(), 1);
        match &actions[0].content {
            ConfigContent::JsonMerge { server_value, .. } => {
                assert!(
                    server_value.get("type").is_none(),
                    "Factory has no type field"
                );
                assert!(server_value.get("url").is_some());
            }
            _ => panic!("expected JsonMerge"),
        }
    }

    #[test]
    fn write_config_atomic_creates_parent_dirs() {
        let tmp = setup_real_tempdir();
        let deep = tmp.path().join("a").join("b").join("c").join("config.json");
        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: deep.clone(),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"hello": "world"})),
            permissions: 0o644,
            backup: false,
        };
        let outcome = write_config_atomic(&action, false).unwrap();
        assert_eq!(outcome, ActionOutcome::Created);
        assert!(deep.exists());
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&deep).unwrap()).unwrap();
        assert_eq!(content["hello"], "world");
    }

    #[test]
    fn write_config_atomic_backs_up_existing() {
        let tmp = setup_real_tempdir();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"old": true}"#).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path,
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"new": true})),
            permissions: 0o644,
            backup: true,
        };
        let outcome = write_config_atomic(&action, false).unwrap();
        assert_eq!(outcome, ActionOutcome::Updated);

        // Check backup file was created
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(entries.len(), 1, "should have one backup file");
    }

    #[test]
    fn write_config_atomic_refuses_to_overwrite_non_utf8_config() {
        let tmp = setup_real_tempdir();
        let path = tmp.path().join("config.json");
        let original = [0xff, 0xfe, 0xfd];
        std::fs::write(&path, original).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path.clone(),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"new": true})),
            permissions: 0o600,
            backup: true,
        };
        let error =
            write_config_atomic(&action, false).expect_err("invalid UTF-8 must fail closed");

        assert!(matches!(
            error,
            SetupError::Io(ref source) if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn write_config_atomic_never_widens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = setup_real_tempdir();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"old": true}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path.clone(),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"new": true})),
            permissions: 0o644,
            backup: true,
        };
        assert_eq!(
            write_config_atomic(&action, false).unwrap(),
            ActionOutcome::Updated
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let backup = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .expect("backup file");
        assert_eq!(
            backup.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_config_atomic_tightens_permissions_even_when_content_is_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = setup_real_tempdir();
        let path = tmp.path().join("config.json");
        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path.clone(),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"secret": "value"})),
            permissions: 0o600,
            backup: true,
        };
        assert_eq!(
            write_config_atomic(&action, false).unwrap(),
            ActionOutcome::Created
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            write_config_atomic(&action, false).unwrap(),
            ActionOutcome::Updated,
            "permission repair is a material update even when bytes already match"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let backups = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            backups[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_config_atomic_rejects_symlinked_target() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside.json");
        let linked = tmp.path().join("config.json");
        std::fs::write(&outside, r#"{"outside": true}"#).unwrap();
        symlink(&outside, &linked).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: linked,
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"new": true})),
            permissions: 0o644,
            backup: true,
        };
        let err = write_config_atomic(&action, false).unwrap_err();

        assert!(err.to_string().contains("must not be a symlink"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            r#"{"outside": true}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_config_atomic_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside_dir = tmp.path().join("outside");
        let linked_dir = tmp.path().join("linked");
        std::fs::create_dir(&outside_dir).unwrap();
        symlink(&outside_dir, &linked_dir).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: linked_dir.join("config.json"),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"new": true})),
            permissions: 0o644,
            backup: false,
        };
        let err = write_config_atomic(&action, false).unwrap_err();

        assert!(
            err.to_string()
                .contains("must not traverse symlinked directories"),
            "{err}"
        );
        assert!(!outside_dir.join("config.json").exists());
    }

    #[test]
    fn write_config_atomic_unchanged_noop() {
        let tmp = setup_real_tempdir();
        let path = tmp.path().join("config.json");

        // Write initial via merge
        let initial =
            merge_mcp_server(None, "mcpServers", "test", json!({"url": "http://a"})).unwrap();
        std::fs::write(&path, &initial).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path,
            description: "test".into(),
            content: ConfigContent::JsonMerge {
                servers_key: "mcpServers",
                server_name: "test",
                server_value: json!({"url": "http://a"}),
                reconcile_omp_user_runtime_lists: false,
            },
            permissions: 0o644,
            backup: false,
        };
        let outcome = write_config_atomic(&action, false).unwrap();
        assert_eq!(outcome, ActionOutcome::Unchanged);
    }

    #[test]
    fn merge_claude_hooks_empty() {
        let result = merge_claude_hooks(None, "my-project", "RedFox").unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert!(doc["hooks"]["SessionStart"].is_array());
        assert!(doc["hooks"]["PreToolUse"].is_array());
        assert!(doc["hooks"]["PostToolUse"].is_array());
        // Verify no secrets embedded
        assert!(!result.contains("TOKEN"), "hooks must not contain secrets");
    }

    #[test]
    fn merge_claude_hooks_preserves_existing() {
        let existing = r#"{"permissions": {"allow": ["Bash"]}, "hooks": {"SessionStart": [{"matcher": "custom", "hooks": [{"type": "command", "command": "echo hi"}]}]}}"#;
        let result = merge_claude_hooks(Some(existing), "proj", "Agent").unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        // User's custom hook preserved
        assert_eq!(doc["permissions"]["allow"][0], "Bash");
        let session_start = doc["hooks"]["SessionStart"].as_array().unwrap();
        assert!(
            session_start
                .iter()
                .any(|e| e.to_string().contains("custom"))
        );
        // Our hooks added
        assert!(
            session_start
                .iter()
                .any(|e| e.to_string().contains("am file_reservations"))
        );
    }

    #[test]
    fn merge_claude_hooks_idempotent() {
        let result1 = merge_claude_hooks(None, "proj", "Fox").unwrap();
        let result2 = merge_claude_hooks(Some(&result1), "proj", "Fox").unwrap();
        assert_eq!(result1, result2);
    }

    #[test]
    fn merge_claude_hooks_replaces_stale() {
        let result1 = merge_claude_hooks(None, "proj", "OldAgent").unwrap();
        let result2 = merge_claude_hooks(Some(&result1), "proj", "NewAgent").unwrap();
        let doc: Value = serde_json::from_str(&result2).unwrap();
        let post_hooks = doc["hooks"]["PostToolUse"].as_array().unwrap();
        let all_text = post_hooks
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all_text.contains("NewAgent"));
        assert!(!all_text.contains("OldAgent"));
    }

    #[test]
    fn save_token_to_env_file_creates() {
        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join(".env");
        save_token_to_env_file(&env_path, "my-token-123").unwrap();
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("HTTP_BEARER_TOKEN=my-token-123"));
    }

    #[test]
    fn save_token_to_env_file_updates() {
        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join(".env");
        let mut f = std::fs::File::create(&env_path).unwrap();
        writeln!(f, "OTHER=value").unwrap();
        writeln!(f, "HTTP_BEARER_TOKEN=old-token").unwrap();
        writeln!(f, "MORE=stuff").unwrap();
        drop(f);

        save_token_to_env_file(&env_path, "new-token").unwrap();
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("HTTP_BEARER_TOKEN=new-token"));
        assert!(!content.contains("old-token"));
        assert!(content.contains("OTHER=value"));
        assert!(content.contains("MORE=stuff"));
        assert!(content.ends_with('\n'), "file must end with newline");
    }

    #[cfg(unix)]
    #[test]
    fn save_token_to_env_file_repairs_exact_content_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join("config.env");
        std::fs::write(&env_path, "HTTP_BEARER_TOKEN=exact-token\n").unwrap();
        std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_token_to_env_file(&env_path, "exact-token").unwrap();

        let mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "idempotent setup must repair secret mode");
        assert_eq!(
            std::fs::read_to_string(&env_path).unwrap(),
            "HTTP_BEARER_TOKEN=exact-token\n"
        );
    }

    #[test]
    fn save_token_to_env_file_refuses_tracked_idempotent_secret() {
        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join("config.env");
        let original = "HTTP_BEARER_TOKEN=already-tracked\n";
        std::fs::write(&env_path, original).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(["add", "--", "config.env"])
                .status()
                .unwrap()
                .success()
        );

        let error = save_token_to_env_file(&env_path, "already-tracked")
            .expect_err("idempotence must not bypass tracked-secret refusal");
        assert!(error.to_string().contains("Git-tracked config"), "{error}");
        assert_eq!(std::fs::read_to_string(env_path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn save_token_to_env_file_replaces_hard_link_without_mutating_peer() {
        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside.env");
        let env_path = tmp.path().join(".env");
        std::fs::write(&outside, "HTTP_BEARER_TOKEN=outside\n").unwrap();
        std::fs::hard_link(&outside, &env_path).unwrap();

        save_token_to_env_file(&env_path, "new-token").unwrap();

        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "HTTP_BEARER_TOKEN=outside\n"
        );
        assert_eq!(
            std::fs::read_to_string(&env_path).unwrap(),
            "HTTP_BEARER_TOKEN=new-token\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_token_to_env_file_detaches_exact_content_hard_link() {
        use std::os::unix::fs::MetadataExt;

        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside.env");
        let env_path = tmp.path().join("config.env");
        let original = "HTTP_BEARER_TOKEN=exact-token\n";
        std::fs::write(&outside, original).unwrap();
        std::fs::hard_link(&outside, &env_path).unwrap();
        assert_eq!(
            std::fs::metadata(&outside).unwrap().ino(),
            std::fs::metadata(&env_path).unwrap().ino(),
            "fixture must begin as one inode"
        );

        save_token_to_env_file(&env_path, "exact-token").unwrap();

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&env_path).unwrap(), original);
        assert_ne!(
            std::fs::metadata(&outside).unwrap().ino(),
            std::fs::metadata(&env_path).unwrap().ino(),
            "idempotent setup must detach the credential path from its peer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_token_to_env_file_rejects_symlinked_target() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside.env");
        let linked = tmp.path().join(".env");
        std::fs::write(&outside, "HTTP_BEARER_TOKEN=outside\n").unwrap();
        symlink(&outside, &linked).unwrap();

        let err = save_token_to_env_file(&linked, "new-token").unwrap_err();

        assert!(err.to_string().contains("must not be a symlink"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "HTTP_BEARER_TOKEN=outside\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_token_to_env_file_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside_dir = tmp.path().join("outside");
        let linked_dir = tmp.path().join("linked");
        std::fs::create_dir(&outside_dir).unwrap();
        symlink(&outside_dir, &linked_dir).unwrap();

        let err = save_token_to_env_file(&linked_dir.join(".env"), "new-token").unwrap_err();

        assert!(
            err.to_string()
                .contains("must not traverse symlinked directories"),
            "{err}"
        );
        assert!(!outside_dir.join(".env").exists());
    }

    #[test]
    fn gitignore_append_idempotent() {
        let tmp = setup_real_tempdir();
        let gi = tmp.path().join(".gitignore");
        std::fs::write(&gi, ".env\n").unwrap();

        let changed = ensure_gitignore_entries(&gi, &[".claude/settings.local.json"]).unwrap();
        assert!(changed);

        let changed2 = ensure_gitignore_entries(&gi, &[".claude/settings.local.json"]).unwrap();
        assert!(!changed2, "second call should be a no-op");

        let content = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(
            content.matches(".claude/settings.local.json").count(),
            1,
            "entry should appear exactly once"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_gitignore_entries_rejects_symlinked_target() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside = tmp.path().join("outside-gitignore");
        let linked = tmp.path().join(".gitignore");
        std::fs::write(&outside, ".env\n").unwrap();
        symlink(&outside, &linked).unwrap();

        let err = ensure_gitignore_entries(&linked, &[".claude/settings.local.json"]).unwrap_err();

        assert!(err.to_string().contains("must not be a symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), ".env\n");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_gitignore_entries_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let tmp = setup_real_tempdir();
        let outside_dir = tmp.path().join("outside");
        let linked_dir = tmp.path().join("linked");
        std::fs::create_dir(&outside_dir).unwrap();
        symlink(&outside_dir, &linked_dir).unwrap();

        let err = ensure_gitignore_entries(
            &linked_dir.join(".gitignore"),
            &[".claude/settings.local.json"],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("must not traverse symlinked directories"),
            "{err}"
        );
        assert!(!outside_dir.join(".gitignore").exists());
    }

    #[test]
    fn parse_agent_list_works() {
        let list = parse_agent_list("claude, cursor, gemini").unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0], AgentPlatform::Claude);
        assert_eq!(list[1], AgentPlatform::Cursor);
        assert_eq!(list[2], AgentPlatform::Gemini);
    }

    #[test]
    fn parse_agent_list_rejects_unknown() {
        let err = parse_agent_list("claude, unknown-thing").unwrap_err();
        assert!(err.to_string().contains("unknown-thing"));
    }

    #[test]
    fn parse_agent_list_deduplicates() {
        let list = parse_agent_list("claude,claude,cursor").unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn agent_platform_all_has_eleven() {
        // 9 original platforms + Antigravity (agy) + Oh My Pi (OMP).
        assert_eq!(AgentPlatform::ALL.len(), 11);
        assert!(AgentPlatform::ALL.contains(&AgentPlatform::Antigravity));
        assert!(AgentPlatform::ALL.contains(&AgentPlatform::Omp));
    }

    #[test]
    fn check_config_file_detects_server() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = merge_mcp_server(
            None,
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://127.0.0.1:8765/mcp/"}),
        )
        .unwrap();
        std::fs::write(&path, &content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(url_matches);

        let (_, wrong_url) = check_config_file(&path, "http://127.0.0.1:9999/mcp/");
        assert!(!wrong_url);
    }

    #[test]
    fn check_config_file_detects_underscore_server_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = r#"{
  "mcpServers": {
    "mcp_agent_mail": {
      "url": "http://127.0.0.1:8765/mcp/"
    }
  }
}
"#;
        std::fs::write(&path, content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(url_matches);
    }

    #[test]
    fn check_config_file_detects_mcp_servers_container() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = r#"{
  "mcp_servers": {
    "mcp-agent-mail": {
      "url": "http://127.0.0.1:8765/mcp/"
    }
  }
}
"#;
        std::fs::write(&path, content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(url_matches);
    }

    #[test]
    fn check_config_file_detects_toml_http_url() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[mcp_servers.mcp_agent_mail]
url = "http://127.0.0.1:8765/api/"
http_headers = { Authorization = "Bearer tok" }
"#,
        )
        .unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/api/");
        assert!(has_server);
        assert!(url_matches);
    }

    // ── br-3h13: Additional setup.rs test coverage ──────────────────

    #[test]
    fn agent_platform_from_slug_all_aliases() {
        // Primary slugs
        assert_eq!(
            AgentPlatform::from_slug("claude"),
            Some(AgentPlatform::Claude)
        );
        assert_eq!(
            AgentPlatform::from_slug("codex"),
            Some(AgentPlatform::Codex)
        );
        assert_eq!(
            AgentPlatform::from_slug("cursor"),
            Some(AgentPlatform::Cursor)
        );
        assert_eq!(
            AgentPlatform::from_slug("gemini"),
            Some(AgentPlatform::Gemini)
        );
        assert_eq!(AgentPlatform::from_slug("omp"), Some(AgentPlatform::Omp));
        assert_eq!(
            AgentPlatform::from_slug("opencode"),
            Some(AgentPlatform::OpenCode)
        );
        assert_eq!(
            AgentPlatform::from_slug("factory"),
            Some(AgentPlatform::FactoryDroid)
        );
        assert_eq!(
            AgentPlatform::from_slug("cline"),
            Some(AgentPlatform::Cline)
        );
        assert_eq!(
            AgentPlatform::from_slug("windsurf"),
            Some(AgentPlatform::Windsurf)
        );
        assert_eq!(
            AgentPlatform::from_slug("github-copilot"),
            Some(AgentPlatform::GithubCopilot)
        );
        // Alias slugs
        assert_eq!(
            AgentPlatform::from_slug("claude-code"),
            Some(AgentPlatform::Claude)
        );
        assert_eq!(
            AgentPlatform::from_slug("codex-cli"),
            Some(AgentPlatform::Codex)
        );
        assert_eq!(
            AgentPlatform::from_slug("gemini-cli"),
            Some(AgentPlatform::Gemini)
        );
        assert_eq!(
            AgentPlatform::from_slug("oh-my-pi"),
            Some(AgentPlatform::Omp)
        );
        // Antigravity (agy) — primary slug + the agy / antigravity-cli aliases
        // (matches the franken-agent-detection connector slug + aliases).
        assert_eq!(
            AgentPlatform::from_slug("antigravity"),
            Some(AgentPlatform::Antigravity)
        );
        assert_eq!(
            AgentPlatform::from_slug("agy"),
            Some(AgentPlatform::Antigravity)
        );
        assert_eq!(
            AgentPlatform::from_slug("antigravity-cli"),
            Some(AgentPlatform::Antigravity)
        );
        assert_eq!(
            AgentPlatform::from_slug("open-code"),
            Some(AgentPlatform::OpenCode)
        );
        assert_eq!(
            AgentPlatform::from_slug("factory-droid"),
            Some(AgentPlatform::FactoryDroid)
        );
        assert_eq!(
            AgentPlatform::from_slug("copilot"),
            Some(AgentPlatform::GithubCopilot)
        );
        // Unknown
        assert_eq!(AgentPlatform::from_slug("vscode"), None);
        assert_eq!(AgentPlatform::from_slug(""), None);
    }

    #[test]
    fn agent_platform_slug_roundtrip() {
        for &p in AgentPlatform::ALL {
            let slug = p.slug();
            assert_eq!(
                AgentPlatform::from_slug(slug),
                Some(p),
                "from_slug(slug()) should roundtrip for {slug}"
            );
        }
    }

    #[test]
    fn agent_platform_display_name_all() {
        let names: Vec<&str> = AgentPlatform::ALL
            .iter()
            .map(|p| p.display_name())
            .collect();
        assert!(names.contains(&"Claude Code"));
        assert!(names.contains(&"Codex CLI"));
        assert!(names.contains(&"Cursor"));
        assert!(names.contains(&"Gemini CLI"));
        assert!(names.contains(&"Oh My Pi (OMP)"));
        assert!(names.contains(&"Antigravity (agy)"));
        assert!(names.contains(&"OpenCode"));
        assert!(names.contains(&"Factory Droid"));
        assert!(names.contains(&"Cline"));
        assert!(names.contains(&"Windsurf"));
        assert!(names.contains(&"GitHub Copilot"));
    }

    #[test]
    fn agent_platform_display_trait_matches_display_name() {
        for &p in AgentPlatform::ALL {
            assert_eq!(format!("{p}"), p.display_name());
        }
    }

    #[test]
    fn setup_params_server_url_format() {
        let params = SetupParams {
            host: "10.0.0.1".into(),
            port: 9000,
            path: "/api/".into(),
            ..Default::default()
        };
        assert_eq!(params.server_url(), "http://10.0.0.1:9000/api/");
    }

    #[test]
    fn setup_params_server_url_normalizes_unspecified_hosts() {
        let params = SetupParams {
            host: "0.0.0.0".into(),
            port: 8765,
            path: "/mcp/".into(),
            ..Default::default()
        };
        assert_eq!(params.server_url(), "http://127.0.0.1:8765/mcp/");

        let params = SetupParams {
            host: "::".into(),
            port: 8765,
            path: "/mcp/".into(),
            ..Default::default()
        };
        assert_eq!(params.server_url(), "http://[::1]:8765/mcp/");

        let params = SetupParams {
            host: "[::]".into(),
            port: 8765,
            path: "/mcp/".into(),
            ..Default::default()
        };
        assert_eq!(params.server_url(), "http://[::1]:8765/mcp/");
    }

    #[test]
    fn setup_params_server_url_brackets_explicit_ipv6_hosts() {
        let params = SetupParams {
            host: "2001:db8::42".into(),
            port: 8765,
            path: "/mcp/".into(),
            ..Default::default()
        };
        assert_eq!(params.server_url(), "http://[2001:db8::42]:8765/mcp/");
    }

    #[test]
    fn setup_params_default_values() {
        let params = SetupParams::default();
        assert_eq!(params.host, "127.0.0.1");
        assert_eq!(params.port, 8765);
        assert_eq!(params.path, "/mcp/");
        assert_eq!(params.token, "");
        assert_eq!(params.project_dir, PathBuf::from("."));
        assert!(params.home_dir_override.is_none());
        assert!(params.omp_user_config_path_override.is_none());
        assert!(!params.dry_run);
        assert!(!params.skip_user_config);
        assert!(!params.skip_hooks);
    }

    #[test]
    fn action_outcome_display_all_variants() {
        assert_eq!(ActionOutcome::Created.to_string(), "created");
        assert_eq!(ActionOutcome::Updated.to_string(), "updated");
        assert_eq!(ActionOutcome::Unchanged.to_string(), "unchanged");
        assert_eq!(ActionOutcome::Skipped.to_string(), "skipped (dry-run)");
        assert_eq!(
            ActionOutcome::BackedUp("/tmp/bak".into()).to_string(),
            "backed up to /tmp/bak"
        );
        assert_eq!(
            ActionOutcome::Failed("disk full".into()).to_string(),
            "FAILED: disk full"
        );
    }

    #[test]
    fn parse_agent_list_empty_string_returns_empty() {
        let list = parse_agent_list("").unwrap();
        assert_eq!(list, [] as [AgentPlatform; 0]);
    }

    #[test]
    fn parse_agent_list_alias_slugs() {
        let list = parse_agent_list("claude-code, codex-cli, copilot, oh-my-pi").unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[0], AgentPlatform::Claude);
        assert_eq!(list[1], AgentPlatform::Codex);
        assert_eq!(list[2], AgentPlatform::GithubCopilot);
        assert_eq!(list[3], AgentPlatform::Omp);
    }

    #[test]
    fn parse_agent_list_case_insensitive() {
        let list = parse_agent_list("Claude, CURSOR, Gemini-CLI").unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0], AgentPlatform::Claude);
        assert_eq!(list[1], AgentPlatform::Cursor);
        assert_eq!(list[2], AgentPlatform::Gemini);
    }

    #[test]
    fn parse_agent_list_trailing_commas() {
        let list = parse_agent_list(",claude,,cursor,").unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn merge_mcp_server_invalid_json_returns_error() {
        let result = merge_mcp_server(
            Some("not valid json"),
            "mcpServers",
            "test",
            json!({"url": "http://a"}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn merge_mcp_server_array_top_level_returns_error() {
        let result = merge_mcp_server(
            Some("[1, 2, 3]"),
            "mcpServers",
            "test",
            json!({"url": "http://a"}),
        );
        assert!(matches!(result.unwrap_err(), SetupError::NotJsonObject));
    }

    #[test]
    fn merge_mcp_server_whitespace_only_treated_as_empty() {
        let result = merge_mcp_server(
            Some("   "),
            "mcpServers",
            "test",
            json!({"url": "http://a"}),
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(doc["mcpServers"]["test"]["url"], "http://a");
    }

    #[test]
    fn save_token_to_env_file_appends_when_no_existing_token() {
        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "OTHER=value\n").unwrap();
        save_token_to_env_file(&env_path, "new-token").unwrap();
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("OTHER=value"));
        assert!(content.contains("HTTP_BEARER_TOKEN=new-token"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn save_token_to_env_file_creates_parent_dirs() {
        let tmp = setup_real_tempdir();
        let env_path = tmp.path().join("deep").join("nested").join(".env");
        save_token_to_env_file(&env_path, "tok").unwrap();
        assert!(env_path.exists());
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert_eq!(content, "HTTP_BEARER_TOKEN=tok\n");
    }

    #[test]
    fn ensure_gitignore_entries_creates_new_file() {
        let tmp = setup_real_tempdir();
        let gi = tmp.path().join(".gitignore");
        let changed = ensure_gitignore_entries(&gi, &[".env", "*.log"]).unwrap();
        assert!(changed);
        let content = std::fs::read_to_string(&gi).unwrap();
        assert!(content.contains(".env"));
        assert!(content.contains("*.log"));
    }

    #[test]
    fn ensure_gitignore_entries_no_trailing_newline_handled() {
        let tmp = setup_real_tempdir();
        let gi = tmp.path().join(".gitignore");
        std::fs::write(&gi, "existing").unwrap(); // no trailing newline
        let changed = ensure_gitignore_entries(&gi, &[".env"]).unwrap();
        assert!(changed);
        let content = std::fs::read_to_string(&gi).unwrap();
        // Should have newline between existing and new entry
        assert!(content.contains("existing\n.env\n"));
    }

    #[test]
    fn claude_config_actions_full_set() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            project_slug: "my-proj".into(),
            agent_name: "RedFox".into(),
            skip_user_config: false,
            skip_hooks: false,
            ..Default::default()
        };
        let actions = AgentPlatform::Claude.config_actions(&params);
        // project-local, user-level, hooks = 3 actions
        assert_eq!(actions.len(), 3);
        // GH#168: project-local MCP config is the local scope inside ~/.claude.json.
        assert!(actions[0].file_path.ends_with(".claude.json"));
        assert!(matches!(
            actions[0].content,
            ConfigContent::ClaudeLocalScopeMcp { .. }
        ));
        // User-level MCP config is the top-level mcpServers inside ~/.claude.json.
        assert!(actions[1].file_path.ends_with(".claude.json"));
        assert!(matches!(
            actions[1].content,
            ConfigContent::JsonMerge {
                servers_key: "mcpServers",
                ..
            }
        ));
        // Hooks still live in the project-local settings.json.
        assert!(actions[2].file_path.ends_with(".claude/settings.json"));
        assert!(matches!(
            actions[2].content,
            ConfigContent::HooksMerge { .. }
        ));
    }

    #[test]
    fn claude_config_actions_skip_user_and_hooks() {
        let params = SetupParams {
            token: "tok".into(),
            project_dir: PathBuf::from("/tmp/p"),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        };
        let actions = AgentPlatform::Claude.config_actions(&params);
        assert_eq!(actions.len(), 1, "only project-local action");
    }

    #[test]
    fn run_setup_dry_run_produces_skipped_outcomes() {
        let tmp = tempfile::tempdir().unwrap();
        let params = SetupParams {
            token: "tok".into(),
            project_dir: tmp.path().to_path_buf(),
            agents: Some(vec![AgentPlatform::Cline]),
            dry_run: true,
            ..Default::default()
        };
        let results = run_setup(&params);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].platform, "Cline");
        for action in &results[0].actions {
            assert_eq!(action.outcome, ActionOutcome::Skipped);
        }
    }

    #[test]
    fn run_setup_creates_gitignore_entries() {
        let tmp = setup_real_tempdir();
        let home = tmp.path().join("home");
        // Cline writes a token-bearing project-local file (cline.mcp.json) that
        // must be gitignored; pair it with Claude to exercise both.
        let params = SetupParams {
            token: "tok".into(),
            project_dir: tmp.path().to_path_buf(),
            // home_dir_override keeps the Claude write (GH#168: ~/.claude.json)
            // off the real home during tests.
            home_dir_override: Some(home.clone()),
            agents: Some(vec![AgentPlatform::Claude, AgentPlatform::Cline]),
            skip_user_config: true,
            skip_hooks: true,
            project_slug: "test".into(),
            agent_name: "RedFox".into(),
            ..Default::default()
        };
        let _ = run_setup(&params);
        let gi = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap_or_default();
        assert!(gi.contains(".env"));
        // Cline's project-local token file is still gitignored.
        assert!(gi.contains("cline.mcp.json"));
        // GH#168: Claude's secret-bearing MCP config lands in ~/.claude.json
        // (under the tmp home), NOT the project dir, and carries the token.
        let claude_json = std::fs::read_to_string(home.join(".claude.json")).unwrap();
        assert!(claude_json.contains("\"tok\"") || claude_json.contains("Bearer tok"));
        assert!(claude_json.contains("mcp-agent-mail"));
        // And nothing token-bearing was written into the project's .claude dir.
        assert!(
            !tmp.path()
                .join(".claude")
                .join("settings.local.json")
                .exists()
        );
    }

    #[test]
    fn hook_is_ours_detects_all_markers() {
        assert!(hook_is_ours(&json!({"command": "mcp-agent-mail serve"})));
        assert!(hook_is_ours(
            &json!({"command": "am file_reservations active proj"})
        ));
        assert!(hook_is_ours(
            &json!({"command": "am acks pending proj agent"})
        ));
        assert!(hook_is_ours(
            &json!({"command": "am mail inbox --project proj"})
        ));
        assert!(!hook_is_ours(&json!({"command": "echo hello"})));
        assert!(!hook_is_ours(&json!({"command": "cargo build"})));
    }

    #[test]
    fn check_config_file_httpurl_key() {
        // Gemini uses httpUrl instead of url
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = merge_mcp_server(
            None,
            "mcpServers",
            "mcp-agent-mail",
            json!({"httpUrl": "http://127.0.0.1:8765/mcp/"}),
        )
        .unwrap();
        std::fs::write(&path, &content).unwrap();
        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(url_matches);
    }

    #[test]
    fn check_config_file_distinguishes_api_and_mcp_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = merge_mcp_server(
            None,
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://127.0.0.1:8765/api/"}),
        )
        .unwrap();
        std::fs::write(&path, &content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(!url_matches);
    }

    #[test]
    fn check_config_file_treats_localhost_as_loopback_equivalent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = merge_mcp_server(
            None,
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://localhost:8765/mcp/"}),
        )
        .unwrap();
        std::fs::write(&path, &content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(url_matches);

        assert!(urls_match_for_status(
            "http://localhost:8765/mcp/",
            "http://[::1]:8765/mcp/"
        ));
    }

    #[test]
    fn check_config_file_keeps_ipv4_and_ipv6_loopback_literals_distinct() {
        assert!(!urls_match_for_status(
            "http://[::1]:8765/mcp/",
            "http://127.0.0.1:8765/mcp/"
        ));
        assert!(!urls_match_for_status(
            "http://127.0.0.1:8765/mcp/",
            "http://[::1]:8765/mcp/"
        ));
    }

    #[test]
    fn check_config_file_custom_path_stays_strict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        let content = merge_mcp_server(
            None,
            "mcpServers",
            "mcp-agent-mail",
            json!({"url": "http://127.0.0.1:8765/custom/"}),
        )
        .unwrap();
        std::fs::write(&path, &content).unwrap();

        let (has_server, url_matches) = check_config_file(&path, "http://127.0.0.1:8765/mcp/");
        assert!(has_server);
        assert!(!url_matches);
    }

    #[test]
    fn check_config_file_nonexistent_returns_false() {
        let (has, matches) = check_config_file(Path::new("/nonexistent/config.json"), "http://a");
        assert!(!has);
        assert!(!matches);
    }

    #[test]
    fn check_config_file_invalid_json_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        let (has, matches) = check_config_file(&path, "http://a");
        assert!(!has);
        assert!(!matches);
    }

    #[test]
    fn check_status_reports_ok_for_all_supported_platforms_in_temp_config_homes() {
        let tmp = setup_real_tempdir();
        let project_dir = tmp.path().join("project");
        let home_dir = tmp.path().join("home");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        let params = SetupParams {
            token: "tok".into(),
            project_dir,
            home_dir_override: Some(home_dir.clone()),
            skip_hooks: true,
            ..Default::default()
        };

        for platform in AgentPlatform::ALL {
            for action in platform.config_actions(&params) {
                if matches!(action.content, ConfigContent::HooksMerge { .. }) {
                    continue;
                }
                write_config_atomic(&action, false).unwrap();
            }
        }

        let statuses = check_status(&params);
        assert_eq!(statuses.len(), AgentPlatform::ALL.len());
        for status in statuses {
            assert!(
                !status.config_files.is_empty(),
                "{} should have config files",
                status.slug
            );
            for file in &status.config_files {
                assert!(file.exists, "{status:?}");
                assert!(file.has_server_entry, "{file:?}");
                assert!(file.url_matches, "{file:?}");
                assert_eq!(file.primary_drift_reason, ConfigDriftReason::Ok);
                assert_eq!(file.risk, ConfigDriftRisk::None);
                assert_eq!(file.remediation, "no action");
                assert!(
                    !serde_json::to_string(&file).unwrap().contains("tok"),
                    "status JSON must redact bearer tokens"
                );
                if file.path.starts_with(&home_dir.display().to_string()) {
                    assert!(
                        file.redacted_path.starts_with("~/"),
                        "home path should be redacted: {}",
                        file.redacted_path
                    );
                }
            }
        }
    }

    #[test]
    fn check_status_reports_missing_file_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        let file = first_setup_status_file(&params);
        assert_eq!(file.primary_drift_reason, ConfigDriftReason::MissingFile);
        assert!(file.drift_reasons.contains(&ConfigDriftReason::MissingFile));
        assert_eq!(file.risk, ConfigDriftRisk::Medium);
    }

    #[test]
    fn check_status_ignores_project_omp_runtime_lists_when_transport_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "disabledServers": ["mcp-agent-mail"],
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true
    }
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);
        assert!(file.url_matches);
        assert!(
            file.drift_reasons.is_empty(),
            "OMP ignores disabledServers outside the active user mcp.json: {file:?}"
        );
        assert_eq!(file.primary_drift_reason, ConfigDriftReason::Ok);
        assert_eq!(file.risk, ConfigDriftRisk::None);
    }

    #[test]
    fn check_status_project_only_honors_active_omp_user_denylist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let project_config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(project_config.parent().unwrap()).unwrap();
        std::fs::write(
            &project_config,
            r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true
    }
  }
}"#,
        )
        .unwrap();

        let home = params.home_dir_override.clone().unwrap();
        let default_user_config = home.join(".omp/agent/mcp.json");
        let active_user_config = home.join(".omp/profiles/work/agent/mcp.json");
        std::fs::create_dir_all(default_user_config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(active_user_config.parent().unwrap()).unwrap();
        std::fs::write(
            default_user_config,
            r#"{"disabledServers":["mcp-agent-mail"]}"#,
        )
        .unwrap();
        std::fs::write(&active_user_config, r#"{"disabledServers":["other"]}"#).unwrap();
        params.omp_user_config_path_override = Some(active_user_config.clone());

        let healthy = first_setup_status_file(&params);
        assert!(healthy.exists && healthy.has_server_entry && healthy.url_matches);
        assert!(
            healthy.drift_reasons.is_empty(),
            "the selected profile, not the default profile, is authoritative"
        );

        std::fs::write(
            &active_user_config,
            r#"{"disabledServers":["mcp-agent-mail"]}"#,
        )
        .unwrap();
        let disabled = first_setup_status_file(&params);
        assert!(
            disabled
                .drift_reasons
                .contains(&ConfigDriftReason::DisabledServer)
        );
        assert_eq!(
            disabled.primary_drift_reason,
            ConfigDriftReason::DisabledServer
        );
        assert_eq!(disabled.risk, ConfigDriftRisk::Medium);
        assert!(
            disabled
                .remediation
                .contains("~/.omp/profiles/work/agent/mcp.json")
        );
        assert!(
            !disabled.remediation.contains("--no-user-config"),
            "repair must include the globally authoritative user config"
        );

        std::fs::write(
            active_user_config,
            r#"{"disabledServers":["mcp-agent-mail",7]}"#,
        )
        .unwrap();
        let malformed = first_setup_status_file(&params);
        assert_eq!(
            malformed.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert!(
            malformed
                .drift_reasons
                .contains(&ConfigDriftReason::DisabledServer)
        );
        assert!(malformed.remediation.starts_with(
            "inspect unsupported active OMP user config ~/.omp/profiles/work/agent/mcp.json"
        ));
    }

    #[test]
    fn check_status_omp_secondary_sources_shadow_same_name_but_reject_distinct_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let project_primary = params.project_dir.join(".omp/mcp.json");
        let project_secondary = params.project_dir.join(".omp/.mcp.json");
        let user_primary = omp_active_user_config_path(&params).unwrap();
        let user_secondary = user_primary.parent().unwrap().join(".mcp.json");
        let root_primary = params.project_dir.join("mcp.json");
        let root_secondary = params.project_dir.join(".mcp.json");
        for path in [
            &project_primary,
            &project_secondary,
            &user_primary,
            &user_secondary,
            &root_primary,
            &root_secondary,
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        let canonical = |url: &str| {
            format!(
                r#"{{"mcpServers":{{"mcp-agent-mail":{{"type":"http","url":"{url}","headers":{{"Authorization":"Bearer tok"}},"enabled":true}}}}}}"#
            )
        };
        std::fs::write(&project_primary, canonical("http://127.0.0.1:8765/mcp/")).unwrap();
        for path in [
            &project_secondary,
            &user_primary,
            &user_secondary,
            &root_primary,
            &root_secondary,
        ] {
            std::fs::write(path, canonical("http://stale.example/mcp")).unwrap();
        }

        let same_name_shadowed = first_setup_status_file(&params);
        assert!(
            same_name_shadowed.drift_reasons.is_empty(),
            "later canonical names are shadowed by the first project mcp.json definition"
        );
        assert!(!same_name_shadowed.omp_mcp_alias_drift);

        std::fs::write(
            &user_secondary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let hidden_user_alias = first_setup_status_file(&params);
        assert_eq!(
            hidden_user_alias.primary_drift_reason,
            ConfigDriftReason::DuplicateServerEntries
        );
        assert!(hidden_user_alias.omp_mcp_alias_drift);
        assert!(
            hidden_user_alias
                .remediation
                .contains("~/.omp/agent/.mcp.json")
        );
        assert!(!hidden_user_alias.remediation.contains("--no-user-config"));

        std::fs::write(&user_secondary, canonical("http://stale.example/mcp")).unwrap();
        std::fs::write(
            &user_primary,
            r#"{"mcpServers":{"agent-mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let primary_user_alias = first_setup_status_file(&params);
        assert!(primary_user_alias.omp_mcp_alias_drift);
        assert!(
            primary_user_alias
                .remediation
                .contains("~/.omp/agent/mcp.json")
        );

        std::fs::write(&user_primary, canonical("http://stale.example/mcp")).unwrap();
        std::fs::write(
            &project_secondary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let hidden_project_alias = first_setup_status_file(&params);
        assert!(hidden_project_alias.omp_mcp_alias_drift);
        assert!(hidden_project_alias.remediation.contains(".omp/.mcp.json"));

        std::fs::write(&project_secondary, canonical("http://stale.example/mcp")).unwrap();
        std::fs::write(
            &root_primary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let root_alias = first_setup_status_file(&params);
        assert!(root_alias.omp_mcp_alias_drift);
        assert!(root_alias.remediation.contains("mcp.json"));

        for disabled in [
            Value::Bool(false),
            Value::String("false".to_string()),
            Value::String("0".to_string()),
        ] {
            std::fs::write(&root_primary, canonical("http://stale.example/mcp")).unwrap();
            std::fs::write(
                &user_secondary,
                serde_json::to_string(&json!({
                    "mcpServers": {
                        "mcp_agent_mail": {
                            "type": "http",
                            "url": "http://stale.example/mcp",
                            "enabled": disabled.clone(),
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            let disabled_alias = first_setup_status_file(&params);
            assert!(
                !disabled_alias.omp_mcp_alias_drift,
                "OMP-native disabled alias {disabled:?} must not be reported as a live duplicate"
            );
        }

        std::fs::write(&user_secondary, canonical("http://stale.example/mcp")).unwrap();
        std::fs::write(
            &root_primary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp","enabled":false}}}"#,
        )
        .unwrap();
        assert!(!first_setup_status_file(&params).omp_mcp_alias_drift);

        std::fs::write(
            &root_primary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp","enabled":"false"}}}"#,
        )
        .unwrap();
        assert!(
            first_setup_status_file(&params).omp_mcp_alias_drift,
            "standalone mcp-json ignores non-boolean enabled values instead of coercing them"
        );

        std::fs::write(
            &root_primary,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","command":"stale"}}}"#,
        )
        .unwrap();
        assert!(
            !first_setup_status_file(&params).omp_mcp_alias_drift,
            "transport/endpoint mismatches cannot become a live connection"
        );
    }

    #[test]
    fn omp_unsupported_provider_authorities_are_never_probed_or_fingerprinted() {
        let tmp = setup_real_tempdir();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        write_healthy_omp_project_config(&params);

        let intermediate = params.project_dir.join("intermediate");
        std::fs::create_dir_all(&intermediate).unwrap();
        let unsafe_target = params.project_dir.join("unsafe-claude-authority");
        std::fs::write(
            &unsafe_target,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let unsafe_override = intermediate.join("../unsafe-claude-authority");
        let _claude_override =
            EnvVarGuard::set("CLAUDE_CONFIG_DIR", unsafe_override.display().to_string());

        assert!(
            unsafe_override.is_file(),
            "the no-probe control must be real"
        );
        assert!(
            !omp_mcp_authority_paths(&params).contains(&unsafe_override),
            "unsafe raw override must never enter cache fingerprints"
        );
        let unsafe_status = first_setup_status_file(&params);
        assert_eq!(
            unsafe_status.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert!(
            unsafe_status
                .status_observations
                .iter()
                .all(|observation| observation.path != unsafe_override.display().to_string()),
            "unsupported raw authority must not be read or observed"
        );
        assert!(
            !unsafe_status
                .drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries),
            "bytes at the rejected raw path must not influence alias liveness"
        );

        drop(_claude_override);
        let missing_home_tmp = setup_real_tempdir();
        let project_dir = missing_home_tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let missing_home_params = SetupParams {
            token: "tok".to_string(),
            project_dir: project_dir.clone(),
            omp_user_config_path_override: Some(
                missing_home_tmp.path().join("active/agent/mcp.json"),
            ),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..SetupParams::default()
        };
        write_healthy_omp_project_config(&missing_home_params);
        let sentinel = project_dir.join("<unresolved-claude-user-home>");
        std::fs::write(
            &sentinel,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp"}}}"#,
        )
        .unwrap();
        let _missing_home = EnvVarGuard::unset(TEST_OMP_HOME_DIR_OVERRIDE_KEY);

        assert!(!omp_mcp_authority_paths(&missing_home_params).contains(&sentinel));
        let missing_home_status = first_setup_status_file(&missing_home_params);
        assert_eq!(
            missing_home_status.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert!(
            missing_home_status
                .status_observations
                .iter()
                .all(|observation| observation.path != sentinel.display().to_string())
        );
        assert!(
            !missing_home_status
                .drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries)
        );
    }

    #[test]
    fn omp_global_runtime_lists_apply_exact_names_deny_wins_and_force_enable() {
        let tmp = setup_real_tempdir();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        write_healthy_omp_project_config(&params);
        let root_alias = params.project_dir.join("mcp.json");
        std::fs::write(
            &root_alias,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://stale.example/mcp","enabled":false}}}"#,
        )
        .unwrap();
        let user = omp_active_user_config_path(&params).unwrap();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();

        std::fs::write(
            &user,
            r#"{"disabledServers":["mcp_agent_mail"],"enabledServers":["mcp_agent_mail"]}"#,
        )
        .unwrap();
        let deny_wins = first_setup_status_file(&params);
        assert!(!deny_wins.omp_mcp_alias_drift);
        assert!(deny_wins.drift_reasons.is_empty());

        std::fs::write(&user, r#"{"enabledServers":["mcp_agent_mail"]}"#).unwrap();
        let forced_live = first_setup_status_file(&params);
        assert!(forced_live.omp_mcp_alias_drift);
        assert!(
            forced_live
                .drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries)
        );

        std::fs::write(&user, r#"{"disabledServers":["mcp_agent_mail"]}"#).unwrap();
        assert!(first_setup_status_file(&params).drift_reasons.is_empty());

        std::fs::write(&user, "{}\n").unwrap();
        assert!(
            first_setup_status_file(&params).drift_reasons.is_empty(),
            "entry-level enabled=false remains suppressed without a force-enable override"
        );
    }

    #[test]
    fn omp_semantic_equivalence_and_provider_transport_filter_match_runtime() {
        let tmp = setup_real_tempdir();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        write_healthy_omp_project_config(&params);
        let root = params.project_dir.join("mcp.json");

        std::fs::write(
            &root,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"}}}}"#,
        )
        .unwrap();
        assert!(
            first_setup_status_file(&params).drift_reasons.is_empty(),
            "an exact connection alias is equivalence-shadowed by the canonical entry"
        );

        std::fs::write(
            &root,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok","X-Route":"other"}}}}"#,
        )
        .unwrap();
        assert!(first_setup_status_file(&params).omp_mcp_alias_drift);

        std::fs::write(
            &root,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://stale.example/shadowed"}}}"#,
        )
        .unwrap();
        let vscode = params.project_dir.join(".vscode/mcp.json");
        std::fs::create_dir_all(vscode.parent().unwrap()).unwrap();
        std::fs::write(
            &vscode,
            r#"{"mcp":{"servers":{"mcp_agent_mail":{"transport":"future-http","url":"http://stale.example/mcp"}}}}"#,
        )
        .unwrap();
        let inferred_http = first_setup_status_file(&params);
        assert!(
            inferred_http.omp_mcp_alias_drift,
            "VS Code drops an unknown transport and then infers HTTP from url"
        );
        assert!(inferred_http.remediation.contains(".vscode/mcp.json"));

        std::fs::write(&vscode, "{}\n").unwrap();
        let _claude_overlap = EnvVarGuard::set(
            "CLAUDE_CONFIG_DIR",
            params.project_dir.display().to_string(),
        );
        std::fs::write(
            params.project_dir.join(".claude.json"),
            r#"{"mcpServers":{"unrelated":{"command":"node"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &root,
            r#"{"mcpServers":{"mcp_agent_mail":{"type":"http","url":"http://standalone.example/mcp"}}}"#,
        )
        .unwrap();
        assert_eq!(
            omp_mcp_authority_paths(&params)
                .iter()
                .filter(|path| **path == root)
                .count(),
            1,
            "cache fingerprints dedupe a physical path even when provider identities do not"
        );
        let overlapping_provider_path = first_setup_status_file(&params);
        assert!(
            overlapping_provider_path.omp_mcp_alias_drift,
            "Claude's skipped overlapping alternative must not erase the later standalone provider identity"
        );
    }

    #[test]
    fn omp_opencode_layers_deep_merge_and_project_filter_before_dedupe() {
        let tmp = setup_real_tempdir();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        write_healthy_omp_project_config(&params);
        let home = params.home_dir_override.as_ref().unwrap();
        let user_layer = home.join(".config/opencode/opencode.json");
        let project_layer = params.project_dir.join("opencode.jsonc");
        std::fs::create_dir_all(user_layer.parent().unwrap()).unwrap();
        std::fs::write(
            &user_layer,
            r#"{"mcp":{"mcp_agent_mail":{"type":"remote","url":"http://stale.example/mcp","headers":{"Authorization":"Bearer tok"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            &project_layer,
            "{\n  // Partial project override inherits the user endpoint.\n  \"mcp\": {\"mcp_agent_mail\": {\"headers\": {\"X-Layer\": \"project\"},},},\n}\n",
        )
        .unwrap();

        let merged = first_setup_status_file(&params);
        assert!(merged.omp_mcp_alias_drift);
        assert!(merged.remediation.contains("opencode.jsonc"));

        let user_settings = home.join(".omp/agent/config.yml");
        std::fs::create_dir_all(user_settings.parent().unwrap()).unwrap();
        std::fs::write(&user_settings, "mcp:\n  enableProjectConfig: false\n").unwrap();
        let project_filtered = first_setup_status_file(&params);
        assert!(!project_filtered.omp_mcp_alias_drift);
        assert!(
            !project_filtered
                .drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries),
            "the merged OpenCode item is project-level because its highest contributing layer is project-level"
        );
        assert_eq!(
            project_filtered.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled
        );
    }

    #[test]
    fn omp_public_planning_and_direct_setup_honor_active_environment_authority() {
        let tmp = setup_real_tempdir();
        let invalid = SetupParams {
            project_dir: tmp.path().join("invalid-project"),
            home_dir_override: Some(PathBuf::from("relative-home")),
            agents: Some(vec![AgentPlatform::Omp]),
            token: "must-not-be-planned".to_string(),
            skip_hooks: true,
            ..SetupParams::default()
        };
        assert!(AgentPlatform::Omp.config_actions(&invalid).is_empty());
        let invalid_status = first_setup_status_file(&invalid);
        assert_eq!(
            invalid_status.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );

        let home = tmp.path().join("home");
        let named_project = tmp.path().join("named-project");
        std::fs::create_dir_all(&named_project).unwrap();
        {
            let _home =
                EnvVarGuard::set(TEST_OMP_HOME_DIR_OVERRIDE_KEY, home.display().to_string());
            let _omp_profile = EnvVarGuard::set("OMP_PROFILE", "work");
            let _pi_profile = EnvVarGuard::unset("PI_PROFILE");
            let _config_dir = EnvVarGuard::set("PI_CONFIG_DIR", ".custom-omp");
            let _agent_dir = EnvVarGuard::set(
                "PI_CODING_AGENT_DIR",
                tmp.path().join("ignored-agent").display().to_string(),
            );
            let params = SetupParams {
                project_dir: named_project.clone(),
                agents: Some(vec![AgentPlatform::Omp]),
                token: "named-profile-token".to_string(),
                skip_hooks: true,
                ..SetupParams::default()
            };
            let results = run_setup(&params);
            assert!(
                results
                    .iter()
                    .flat_map(|result| &result.actions)
                    .all(|action| { !matches!(action.outcome, ActionOutcome::Failed(_)) })
            );
        }
        let named_authority = home.join(".custom-omp/profiles/work/agent/mcp.json");
        assert!(named_authority.is_file());
        assert!(!home.join(".omp/agent/mcp.json").exists());

        let custom_project = tmp.path().join("custom-agent-project");
        let custom_agent = tmp.path().join("custom-agent");
        std::fs::create_dir_all(&custom_project).unwrap();
        {
            let _home =
                EnvVarGuard::set(TEST_OMP_HOME_DIR_OVERRIDE_KEY, home.display().to_string());
            let _omp_profile = EnvVarGuard::unset("OMP_PROFILE");
            let _pi_profile = EnvVarGuard::unset("PI_PROFILE");
            let _config_dir = EnvVarGuard::unset("PI_CONFIG_DIR");
            let _agent_dir =
                EnvVarGuard::set("PI_CODING_AGENT_DIR", custom_agent.display().to_string());
            let params = SetupParams {
                project_dir: custom_project,
                agents: Some(vec![AgentPlatform::Omp]),
                token: "custom-agent-token".to_string(),
                skip_hooks: true,
                ..SetupParams::default()
            };
            let results = run_setup(&params);
            assert!(
                results
                    .iter()
                    .flat_map(|result| &result.actions)
                    .all(|action| { !matches!(action.outcome, ActionOutcome::Failed(_)) })
            );
        }
        assert!(custom_agent.join("mcp.json").is_file());
        assert!(!home.join(".omp/agent/mcp.json").exists());
    }

    #[test]
    fn check_status_project_only_honors_effective_omp_project_config_setting() {
        let tmp = tempfile::tempdir().unwrap();
        let mut params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let project_mcp = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(project_mcp.parent().unwrap()).unwrap();
        std::fs::write(
            &project_mcp,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"},"enabled":true}}}"#,
        )
        .unwrap();
        let user_settings = params
            .home_dir_override
            .as_ref()
            .unwrap()
            .join(".omp/agent/config.yml");
        let project_settings = params.project_dir.join(".omp/config.yml");
        std::fs::create_dir_all(user_settings.parent().unwrap()).unwrap();

        let default_enabled = first_setup_status_file(&params);
        assert!(default_enabled.drift_reasons.is_empty());
        assert!(!default_enabled.omp_settings_config_drift);
        assert!(
            default_enabled
                .status_observations
                .iter()
                .any(|observation| {
                    observation.path == project_settings.display().to_string()
                        && !observation.exists
                })
        );

        std::fs::write(&user_settings, "mcp:\n  enableProjectConfig: false\n").unwrap();
        let user_disabled = first_setup_status_file(&params);
        assert_eq!(
            user_disabled.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled
        );
        assert!(user_disabled.omp_settings_config_drift);
        assert!(user_disabled.remediation.contains(
            "effective OMP setting mcp.enableProjectConfig=false from ~/.omp/agent/config.yml"
        ));
        assert!(!user_disabled.remediation.contains("--no-user-config"));

        std::fs::write(&project_settings, "mcp:\n  enableProjectConfig: true\n").unwrap();
        let project_override = first_setup_status_file(&params);
        assert!(
            project_override.drift_reasons.is_empty(),
            "project settings override the active profile's global setting"
        );

        std::fs::write(&project_settings, "mcp:\n  enableProjectConfig: false\n").unwrap();
        let project_disabled = first_setup_status_file(&params);
        assert_eq!(
            project_disabled.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled
        );
        assert!(project_disabled.remediation.contains(".omp/config.yml"));

        let first_overlay = tmp.path().join("first-overlay.yml");
        let second_overlay = tmp.path().join("second overlay.yml");
        std::fs::write(&first_overlay, "mcp:\n  enableProjectConfig: true\n").unwrap();
        std::fs::write(&second_overlay, "mcp:\n  enableProjectConfig: false\n").unwrap();
        params.omp_settings_overlay_paths = vec![first_overlay, second_overlay.clone()];
        let overlay_disabled = first_setup_status_file(&params);
        assert_eq!(
            overlay_disabled.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled
        );
        assert!(overlay_disabled.remediation.contains("second overlay.yml"));

        std::fs::write(&second_overlay, "mcp:\n  enableProjectConfig: true\n").unwrap();
        let final_overlay_override = first_setup_status_file(&params);
        assert!(
            final_overlay_override.drift_reasons.is_empty(),
            "later overlays override project and earlier overlay settings"
        );
    }

    #[test]
    fn check_status_omp_settings_fallback_profile_and_parse_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let project_mcp = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(project_mcp.parent().unwrap()).unwrap();
        std::fs::write(
            project_mcp,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"},"enabled":true}}}"#,
        )
        .unwrap();
        let home = params.home_dir_override.as_ref().unwrap();
        let default_settings = home.join(".omp/agent/config.yml");
        let active_user_mcp = home.join(".omp/profiles/work/agent/mcp.json");
        let active_yml = home.join(".omp/profiles/work/agent/config.yml");
        let active_yaml = home.join(".omp/profiles/work/agent/config.yaml");
        std::fs::create_dir_all(default_settings.parent().unwrap()).unwrap();
        std::fs::create_dir_all(active_yaml.parent().unwrap()).unwrap();
        std::fs::write(&default_settings, "mcp:\n  enableProjectConfig: true\n").unwrap();
        std::fs::write(&active_yaml, "mcp:\n  enableProjectConfig: false\n").unwrap();
        params.omp_user_config_path_override = Some(active_user_mcp);

        let named_fallback = first_setup_status_file(&params);
        assert_eq!(
            named_fallback.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled
        );
        assert!(
            named_fallback
                .remediation
                .contains("~/.omp/profiles/work/agent/config.yaml")
        );

        std::fs::write(&active_yml, "mcp:\n  enableProjectConfig: true\n").unwrap();
        assert!(first_setup_status_file(&params).drift_reasons.is_empty());

        std::fs::write(
            &active_yml,
            "defaults: &disabled\n  enableProjectConfig: false\nmcp:\n  <<: *disabled\n",
        )
        .unwrap();
        assert_eq!(
            first_setup_status_file(&params).primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled,
            "YAML merge keys must affect the same effective leaf as OMP's Bun YAML loader"
        );

        std::fs::write(&active_yml, "mcp: [\n").unwrap();
        let malformed = first_setup_status_file(&params);
        assert_eq!(
            malformed.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert!(malformed.omp_settings_config_drift);
        assert!(malformed.remediation.contains("config.yml"));

        std::fs::write(&active_yml, "mcp:\n  enableProjectConfig: true\n").unwrap();
        let missing_overlay = tmp.path().join("missing-overlay.yml");
        params.omp_settings_overlay_paths = vec![missing_overlay.clone()];
        let missing = first_setup_status_file(&params);
        assert_eq!(
            missing.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert!(
            missing
                .remediation
                .contains(&missing_overlay.display().to_string())
        );

        params.agents = Some(vec![AgentPlatform::Cline]);
        let non_omp = first_setup_status_file(&params);
        assert!(!non_omp.omp_settings_config_drift);
        assert!(
            !non_omp
                .status_observations
                .iter()
                .any(|observation| { observation.path == missing_overlay.display().to_string() })
        );
    }

    #[test]
    fn check_status_omp_honors_every_project_settings_provider_format() {
        let cases = [
            (
                ".omp/settings.json",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
            (".omp/config.yml", "mcp:\n  enableProjectConfig: false\n"),
            (
                ".claude/settings.json",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
            (".codex/config.toml", "[mcp]\nenableProjectConfig = false\n"),
            (
                ".gemini/settings.json",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
            (
                "opencode.json",
                "{\n  // JSONC is accepted even with a .json name\n  \"mcp\": {\"enableProjectConfig\": false,},\n}\n",
            ),
            ("opencode.jsonc", r#"{"mcp":{"enableProjectConfig":false}}"#),
            (
                ".opencode/opencode.json",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
            (
                ".opencode/opencode.jsonc",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
            (
                ".cursor/settings.json",
                r#"{"mcp":{"enableProjectConfig":false}}"#,
            ),
        ];

        for (relative_path, settings) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
            let project_mcp = params.project_dir.join(".omp/mcp.json");
            std::fs::create_dir_all(project_mcp.parent().unwrap()).unwrap();
            std::fs::write(
                project_mcp,
                r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"},"enabled":true}}}"#,
            )
            .unwrap();
            let source = params.project_dir.join(relative_path);
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, settings).unwrap();

            let status = first_setup_status_file(&params);
            assert_eq!(
                status.primary_drift_reason,
                ConfigDriftReason::ProjectConfigDisabled,
                "provider source {relative_path} must govern the effective OMP setting"
            );
            assert!(status.omp_settings_config_drift);
            assert!(status.remediation.contains(relative_path));
            assert!(status.status_observations.iter().any(|observation| {
                observation.path == source.display().to_string() && observation.exists
            }));
        }
    }

    #[test]
    fn check_status_omp_project_provider_precedence_and_invalid_skip_match_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let mut params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let project_mcp = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(project_mcp.parent().unwrap()).unwrap();
        std::fs::write(
            project_mcp,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"},"enabled":true}}}"#,
        )
        .unwrap();
        let native_legacy = params.project_dir.join(".omp/settings.json");
        let native_yaml = params.project_dir.join(".omp/config.yml");
        let codex = params.project_dir.join(".codex/config.toml");
        let cursor = params.project_dir.join(".cursor/settings.json");
        for path in [&native_legacy, &native_yaml, &codex, &cursor] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&native_legacy, r#"{"mcp":{"enableProjectConfig":false}}"#).unwrap();
        std::fs::write(&native_yaml, "mcp:\n  enableProjectConfig: true\n").unwrap();
        std::fs::write(&codex, "[mcp]\nenableProjectConfig = false\n").unwrap();
        std::fs::write(&cursor, r#"{"mcp":{"enableProjectConfig":true}}"#).unwrap();

        let cursor_wins = first_setup_status_file(&params);
        assert!(
            cursor_wins.drift_reasons.is_empty(),
            "lower-priority providers are merged later, so Cursor overrides Codex and native settings"
        );

        std::fs::write(&cursor, "not json").unwrap();
        let invalid_cursor_is_skipped = first_setup_status_file(&params);
        assert_eq!(
            invalid_cursor_is_skipped.primary_drift_reason,
            ConfigDriftReason::ProjectConfigDisabled,
            "foreign-provider parse failures are warned and skipped, exposing the prior Codex value"
        );
        assert!(
            invalid_cursor_is_skipped
                .remediation
                .contains(".codex/config.toml")
        );

        let opencode = params.project_dir.join("opencode.jsonc");
        std::fs::write(
            &opencode,
            r#"{"mcp":{"enableProjectConfig":"{env:OMP_PROJECT_CONFIG}"}}"#,
        )
        .unwrap();
        let dynamic_authority = first_setup_status_file(&params);
        assert_eq!(
            dynamic_authority.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig,
            "a dynamic OpenCode value that can change project-source authority must fail closed rather than enter the cache"
        );

        std::fs::write(
            &opencode,
            r#"{"apiKey":"{env:UNRELATED_KEY}","mcp":{"enableProjectConfig":true}}"#,
        )
        .unwrap();
        assert_eq!(
            first_setup_status_file(&params).primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig,
            "even currently unrelated substitutions can change parsed structure and must not enter the cache without dependency fingerprints"
        );

        std::fs::write(
            &opencode,
            r#"{"apiKey":"literal","mcp":{"enableProjectConfig":true}}"#,
        )
        .unwrap();
        let overlay = tmp.path().join("overlay.yml");
        std::fs::write(&overlay, "mcp:\n  enableProjectConfig: true\n").unwrap();
        params.omp_settings_overlay_paths = vec![overlay];
        assert!(
            first_setup_status_file(&params).drift_reasons.is_empty(),
            "PI_CONFIG_FILES overlays merge after every project provider"
        );
    }

    #[test]
    fn setup_status_remediation_shell_quotes_untrusted_arguments() {
        let home = PathBuf::from("/home/tester");
        let params = SetupParams {
            host: "host;$(touch bad)".to_string(),
            path: "/mcp path/$HOME/'".to_string(),
            project_dir: home.join("project dir/$(touch bad)'"),
            home_dir_override: Some(home),
            agents: Some(vec![AgentPlatform::Omp]),
            skip_user_config: true,
            skip_hooks: true,
            ..SetupParams::default()
        };
        let action = AgentPlatform::Omp
            .config_actions(&params)
            .into_iter()
            .next()
            .expect("OMP project config action");
        let args = setup_status_command_args(&action, &params, true);
        let quoted_host = shell_quote_status_arg(&params.host);
        let quoted_path = shell_quote_status_arg(&params.path);
        let quoted_project = shell_quote_status_path("~/project dir/$(touch bad)'");

        assert_eq!(quoted_host, "'host;$(touch bad)'");
        assert_eq!(quoted_path, "'/mcp path/$HOME/'\"'\"''");
        assert_eq!(quoted_project, "~/'project dir/$(touch bad)'\"'\"''");
        assert!(args.contains(&format!("--host {quoted_host} ")));
        assert!(args.contains(&format!("--path {quoted_path} ")));
        assert!(args.contains(&format!("--project-dir {quoted_project} ")));
        assert!(args.ends_with("--no-user-config --no-hooks"));
        assert_eq!(shell_quote_status_path("~"), "~");
        assert_eq!(shell_quote_status_path("~/safe/path"), "~/safe/path");
    }

    #[test]
    fn check_status_rejects_legacy_only_omp_container() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "servers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"}
    }
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);
        assert!(!file.has_server_entry);
        assert!(!file.url_matches);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::MissingServerEntry
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::MissingServerEntry)
        );
    }

    #[test]
    fn check_status_requires_the_canonical_omp_native_server_name() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "mcpServers": {
    "agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true
    }
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);
        assert!(file.has_server_entry && file.url_matches);
        assert_eq!(file.entry_locations, vec!["mcpServers.agent-mail"]);
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::UnsupportedConfig),
            "historical alias must remain migration input, not healthy authority"
        );
    }

    #[test]
    fn check_status_reports_omp_native_and_legacy_alias_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true
    },
    "agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true
    }
  },
  "servers": {
    "agent-mail": {"command": "stale"}
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);
        assert_eq!(
            file.entry_locations,
            vec!["mcpServers.mcp-agent-mail", "mcpServers.agent-mail"]
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries)
        );
    }

    #[test]
    fn check_status_uses_only_omp_native_entry_for_url_and_auth_health() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:9999/wrong/",
      "headers": {"Authorization": "Bearer stale"},
      "enabled": true
    }
  },
  "servers": {
    "mcp_agent_mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"}
    }
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);
        assert!(
            !file.url_matches,
            "ignored legacy entry must not satisfy URL health"
        );
        assert_eq!(
            file.actual_url.as_deref(),
            Some("http://127.0.0.1:9999/wrong/")
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::DuplicateServerEntries)
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::StaleHttpPath)
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::WrongBearerHeader)
        );
        assert_eq!(
            file.current_entry
                .as_ref()
                .and_then(|entry| entry.get("container"))
                .and_then(Value::as_str),
            Some("mcpServers")
        );
    }

    #[test]
    fn check_status_reports_omp_oauth_override_even_when_bearer_matches() {
        let tmp = setup_real_tempdir();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Omp);
        let config = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config,
            r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp/",
      "headers": {"Authorization": "Bearer tok"},
      "enabled": true,
      "auth": {"type": "oauth", "credentialId": "stale-credential"},
      "oauth": {"clientId": "stale-client"}
    }
  }
}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);

        assert!(file.url_matches);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::WrongBearerHeader
        );
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::WrongBearerHeader),
            "OMP OAuth metadata can replace a matching file bearer at runtime"
        );
    }

    #[test]
    fn check_status_reports_legacy_stdio_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        std::fs::write(
            params.project_dir.join("cline.mcp.json"),
            r#"{"mcpServers":{"mcp-agent-mail":{"command":"mcp-agent-mail","args":[],"transport":"stdio"}}}"#,
        )
        .unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(file.primary_drift_reason, ConfigDriftReason::LegacyStdio);
        assert!(file.drift_reasons.contains(&ConfigDriftReason::LegacyStdio));
        assert!(!file.url_matches);
        assert!(file.remediation.contains("am setup run --dry-run"));
        assert!(file.remediation.contains("am setup run --yes"));
    }

    #[test]
    fn check_status_reports_stale_http_path_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        std::fs::write(
            params.project_dir.join("cline.mcp.json"),
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/api/","headers":{"Authorization":"Bearer tok"}}}}"#,
        )
        .unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(file.primary_drift_reason, ConfigDriftReason::StaleHttpPath);
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::StaleHttpPath)
        );
        assert_eq!(
            file.actual_url.as_deref(),
            Some("http://127.0.0.1:8765/api/")
        );
    }

    #[test]
    fn check_status_reports_wrong_bearer_header_drift_with_redacted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        std::fs::write(
            params.project_dir.join("cline.mcp.json"),
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer wrong"}}}}"#,
        )
        .unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::WrongBearerHeader
        );
        assert_eq!(file.risk, ConfigDriftRisk::High);
        let serialized = serde_json::to_string(&file).unwrap();
        assert!(serialized.contains("Bearer <redacted>"));
        assert!(!serialized.contains("Bearer wrong"));
        assert!(!serialized.contains("Bearer tok"));
    }

    #[test]
    fn check_status_reports_unexpected_bearer_header_in_no_auth_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        params.token.clear();
        std::fs::write(
            params.project_dir.join("cline.mcp.json"),
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"authorization":"Bearer stale"}}}}"#,
        )
        .unwrap();

        let file = first_setup_status_file(&params);

        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::WrongBearerHeader
        );
        assert!(file.url_matches);
        let serialized = serde_json::to_string(&file).unwrap();
        assert!(serialized.contains("Bearer <redacted>"));
        assert!(!serialized.contains("Bearer stale"));
    }

    #[test]
    fn check_status_reports_duplicate_server_entries_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        std::fs::write(
            params.project_dir.join("cline.mcp.json"),
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"}},"mcp_agent_mail":{"url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"}}}}"#,
        )
        .unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::DuplicateServerEntries
        );
        assert_eq!(file.entry_locations.len(), 2);
    }

    #[test]
    fn setup_normalizes_quoted_toml_server_name_alias() {
        let merged = merge_toml_section(
            Some(
                "[mcp_servers.\"mcp-agent-mail\"]\n\
                 url = \"http://127.0.0.1:8765/api/\"\n\
                 startup_timeout_sec = 5\n",
            ),
            "[mcp_servers.mcp_agent_mail]",
            &[
                (
                    "url".to_string(),
                    "\"http://127.0.0.1:8765/mcp/\"".to_string(),
                ),
                ("startup_timeout_sec".to_string(), "15".to_string()),
            ],
        );

        assert!(merged.contains("[mcp_servers.mcp_agent_mail]"));
        assert!(!merged.contains("mcp-agent-mail"));
        assert!(merged.contains("url = \"http://127.0.0.1:8765/mcp/\""));
        assert!(merged.contains("startup_timeout_sec = 15"));
    }

    #[test]
    fn setup_toml_http_migration_removes_stale_transport_and_auth_keys() {
        let merged = merge_toml_section(
            Some(
                "[mcp_servers.mcp_agent_mail]\n\
                 command = \"mcp-agent-mail\"\n\
                 args = [\"serve\"]\n\
                 cwd = \"/stale\"\n\
                 env = { TOKEN = \"stale\" }\n\
                 transport = \"stdio\"\n\
                 http_headers = { Authorization = \"Bearer stale\" }\n\
                 env_http_headers = { Authorization = \"TOKEN_ENV\" }\n\
                 bearer_token_env_var = \"TOKEN_ENV\"\n\
                 tool_timeout_sec = 45\n",
            ),
            "[mcp_servers.mcp_agent_mail]",
            &[
                (
                    "url".to_string(),
                    "\"http://127.0.0.1:8765/mcp/\"".to_string(),
                ),
                ("startup_timeout_sec".to_string(), "30".to_string()),
            ],
        );

        for stale_key in [
            "command",
            "args",
            "cwd",
            "env",
            "transport",
            "http_headers",
            "env_http_headers",
            "bearer_token_env_var",
        ] {
            assert!(
                !merged.lines().any(|line| {
                    line.split_once('=')
                        .is_some_and(|(key, _)| key.trim() == stale_key)
                }),
                "stale setup-owned key remained: {stale_key}\n{merged}"
            );
        }
        assert!(merged.contains("url = \"http://127.0.0.1:8765/mcp/\""));
        assert!(merged.contains("startup_timeout_sec = 30"));
        assert!(merged.contains("tool_timeout_sec = 45"));
    }

    #[test]
    fn setup_coalesces_both_toml_server_name_aliases_without_duplicate_table_headers() {
        let merged = merge_toml_section(
            Some(
                "[mcp_servers.mcp_agent_mail]\n\
                 url = \"http://127.0.0.1:8765/old/\"\n\
                 custom_setting = \"keep-first\"\n\
                 \n\
                 [mcp_servers.\"mcp-agent-mail\"]\n\
                 url = \"http://127.0.0.1:8765/stale/\"\n\
                 custom_setting = \"drop-duplicate\"\n\
                 \n\
                 [other]\n\
                 enabled = true\n",
            ),
            "[mcp_servers.mcp_agent_mail]",
            &[
                (
                    "url".to_string(),
                    "\"http://127.0.0.1:8765/mcp/\"".to_string(),
                ),
                ("startup_timeout_sec".to_string(), "15".to_string()),
            ],
        );

        // TOML rejects a table header when the same table was already
        // declared. One canonical header proves setup did not emit the invalid
        // duplicate-table form that prompted this regression.
        assert_eq!(
            merged.matches("[mcp_servers.mcp_agent_mail]").count(),
            1,
            "only the canonical table header may remain"
        );
        assert!(!merged.contains("[mcp_servers.\"mcp-agent-mail\"]"));
        let sections = collect_toml_server_sections(&merged);
        assert_eq!(sections.len(), 1, "setup status must find one server entry");
        let canonical = &sections[0];
        assert_eq!(canonical.url.as_deref(), Some("http://127.0.0.1:8765/mcp/"));
        assert_eq!(canonical.startup_timeout, Some(15));
        assert!(merged.contains("custom_setting = \"keep-first\""));
        assert!(!merged.contains("custom_setting = \"drop-duplicate\""));
        assert!(merged.contains("[other]\nenabled = true"));
    }

    #[test]
    fn check_status_reports_unsupported_config_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Cline);
        std::fs::write(params.project_dir.join("cline.mcp.json"), "not json").unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::UnsupportedConfig
        );
        assert_eq!(file.risk, ConfigDriftRisk::High);
        assert!(file.remediation.starts_with("inspect unsupported config"));
    }

    #[test]
    fn check_status_reports_wrong_codex_timeout_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let params = setup_status_test_params(tmp.path(), AgentPlatform::Codex);
        let codex_config = params
            .home_dir_override
            .as_ref()
            .unwrap()
            .join(".codex")
            .join("config.toml");
        std::fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
        std::fs::write(
            codex_config,
            r#"[mcp_servers.mcp_agent_mail]
url = "http://127.0.0.1:8765/mcp/"
startup_timeout_sec = 5
http_headers = { Authorization = "Bearer tok" }
"#,
        )
        .unwrap();
        let file = first_setup_status_file(&params);
        assert_eq!(
            file.primary_drift_reason,
            ConfigDriftReason::WrongStartupTimeout
        );
        assert_eq!(file.risk, ConfigDriftRisk::Low);
        assert!(
            file.drift_reasons
                .contains(&ConfigDriftReason::WrongStartupTimeout)
        );
    }

    fn setup_status_test_params(root: &Path, platform: AgentPlatform) -> SetupParams {
        let project_dir = root.join("project");
        let home_dir = root.join("home");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        SetupParams {
            token: "tok".into(),
            project_dir,
            home_dir_override: Some(home_dir),
            agents: Some(vec![platform]),
            skip_user_config: true,
            skip_hooks: true,
            ..Default::default()
        }
    }

    fn write_healthy_omp_project_config(params: &SetupParams) {
        let path = params.project_dir.join(".omp/mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            r#"{"mcpServers":{"mcp-agent-mail":{"type":"http","url":"http://127.0.0.1:8765/mcp/","headers":{"Authorization":"Bearer tok"},"enabled":true}}}"#,
        )
        .unwrap();
    }

    fn first_setup_status_file(params: &SetupParams) -> ConfigFileStatus {
        let mut statuses = check_status(params);
        let status = statuses.pop().unwrap();
        status.config_files.into_iter().next().unwrap()
    }

    #[test]
    fn setup_error_display() {
        let io_err = SetupError::Io(std::io::Error::other("disk full"));
        assert!(io_err.to_string().contains("disk full"));

        let json_err: serde_json::Error = serde_json::from_str::<i32>("bad").unwrap_err();
        let json_setup_err = SetupError::Json(json_err);
        assert!(json_setup_err.to_string().contains("json parse error"));

        assert_eq!(
            SetupError::NotJsonObject.to_string(),
            "expected JSON object at top level or servers key"
        );

        assert!(
            SetupError::UnknownPlatform("foo".into())
                .to_string()
                .contains("foo")
        );

        assert_eq!(SetupError::Other("oops".into()).to_string(), "oops");
    }

    #[test]
    fn agent_platform_serde_roundtrip() {
        for &p in AgentPlatform::ALL {
            let json = serde_json::to_string(&p).unwrap();
            let back: AgentPlatform = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn agent_platform_serde_kebab_case() {
        assert_eq!(
            serde_json::to_string(&AgentPlatform::GithubCopilot).unwrap(),
            "\"github-copilot\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPlatform::FactoryDroid).unwrap(),
            "\"factory-droid\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPlatform::OpenCode).unwrap(),
            "\"open-code\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPlatform::Omp).unwrap(),
            "\"omp\""
        );
    }

    // ── Sender identity verification helpers (issue #42) ─────────────

    #[test]
    fn generate_registration_token_is_43_chars_base64url() {
        let token = generate_registration_token().expect("token generation should succeed");
        // 32 bytes => ceil(32*4/3) = 43 chars without padding
        assert_eq!(token.len(), 43);
        // Only base64url characters (no +, /, or =)
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn generate_registration_token_unique() {
        let a = generate_registration_token().expect("first token generation should succeed");
        let b = generate_registration_token().expect("second token generation should succeed");
        assert_ne!(a, b, "two consecutive tokens must differ");
    }

    #[test]
    fn generate_registration_token_reports_rng_failure() {
        let _guard = RandomFailureGuard::enable();
        let error = generate_registration_token().expect_err("rng failure should surface");
        assert!(error.to_string().contains("CSPRNG failure"));
    }

    #[test]
    fn constant_time_eq_equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_different_slices() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellx"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"hi", b"hello"));
    }

    #[test]
    fn constant_time_str_eq_works() {
        assert!(constant_time_str_eq("abc123", "abc123"));
        assert!(!constant_time_str_eq("abc123", "abc124"));
        assert!(!constant_time_str_eq("abc123", "abc12"));
    }

    #[test]
    fn base64url_encode_nopad_known_vector() {
        // Known test vector: [0, 1, 2] => "AAEC" (standard base64)
        // In base64url: same since no + or / needed
        let result = base64url_encode_nopad(&[0, 1, 2]);
        assert_eq!(result, "AAEC");
    }

    #[test]
    fn base64url_encode_nopad_empty() {
        assert_eq!(base64url_encode_nopad(&[]), "");
    }

    #[test]
    fn base64url_encode_nopad_single_byte() {
        // 0xFF => base64url of [255] = "/w" in standard, "_w" in url-safe
        let result = base64url_encode_nopad(&[0xFF]);
        assert_eq!(result, "_w");
    }
}
