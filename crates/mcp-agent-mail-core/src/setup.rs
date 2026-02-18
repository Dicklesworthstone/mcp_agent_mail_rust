//! Native agent discovery and MCP configuration for `am setup`.
//!
//! Contains agent-agnostic logic: agent registry, config format definitions,
//! token management, JSON merge, atomic file writes. Lives in core (not cli)
//! so it can be reused by the server or tests.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;
use std::path::{Path, PathBuf};

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
            Self::OpenCode => "OpenCode",
            Self::FactoryDroid => "Factory Droid",
            Self::Cline => "Cline",
            Self::Windsurf => "Windsurf",
            Self::GithubCopilot => "GitHub Copilot",
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
    },
    /// Write complete JSON (for new files only).
    JsonFull(Value),
    /// Merge Claude Code hooks into settings.json.
    HooksMerge {
        project_slug: String,
        agent_name: String,
    },
}

/// Parameters driving the setup.
pub struct SetupParams {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub token: String,
    pub project_dir: PathBuf,
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
        format!("http://{}:{}{}", self.host, self.port, self.path)
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
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("RNG failure");
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        use std::fmt::Write;
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Resolve the bearer token from multiple sources in priority order:
/// explicit flag > `HTTP_BEARER_TOKEN` env var > .env file > generate new.
#[must_use]
pub fn resolve_token(explicit: Option<&str>, env_file: &Path) -> String {
    if let Some(t) = explicit
        && !t.is_empty()
    {
        return t.to_string();
    }
    if let Ok(t) = std::env::var("HTTP_BEARER_TOKEN")
        && !t.is_empty()
    {
        return t;
    }
    if let Some(t) = read_env_file_token(env_file) {
        return t;
    }
    generate_token()
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
    let content = if env_path.exists() {
        let existing = std::fs::read_to_string(env_path)?;
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
    } else {
        format!("HTTP_BEARER_TOKEN={token}\n")
    };

    if let Some(parent) = env_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(env_path, content)?;
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
    let existing = std::fs::read_to_string(gitignore_path).unwrap_or_default();
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

    if let Some(parent) = gitignore_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(gitignore_path, content)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Per-agent config generation
// ---------------------------------------------------------------------------

/// Build the standard MCP server JSON value for HTTP agents.
fn standard_http_server_value(url: &str, token: &str) -> Value {
    json!({
        "type": "http",
        "url": url,
        "headers": {
            "Authorization": format!("Bearer {token}")
        }
    })
}

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
        },
        permissions: 0o600,
        backup: true,
    }
}

impl AgentPlatform {
    /// Generate config file actions for this platform.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn config_actions(self, params: &SetupParams) -> Vec<ConfigAction> {
        let url = params.server_url();
        let token = &params.token;
        let pdir = &params.project_dir;
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));

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
            Self::Codex => vec![project_local_action(
                self,
                pdir,
                "codex.mcp.json",
                "mcpServers",
                standard_http_server_value(&url, token),
                "Codex CLI project-local MCP config",
            )],
            Self::Gemini => self.gemini_actions(params, &url, token, pdir, &home),
            Self::OpenCode => vec![project_local_action(
                self,
                pdir,
                "opencode.json",
                "mcp",
                json!({
                    "type": "remote",
                    "url": url,
                    "headers": { "Authorization": format!("Bearer {token}") },
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
        let mut actions = vec![ConfigAction {
            platform: self,
            file_path: pdir.join(".claude").join("settings.local.json"),
            description: "Claude Code project-local MCP config (secrets)".into(),
            content: ConfigContent::JsonMerge {
                servers_key: "mcpServers",
                server_name: "mcp-agent-mail",
                server_value: standard_http_server_value(url, token),
            },
            permissions: 0o600,
            backup: true,
        }];

        if !params.skip_user_config {
            actions.push(ConfigAction {
                platform: self,
                file_path: home.join(".claude").join("settings.json"),
                description: "Claude Code user-level MCP config".into(),
                content: ConfigContent::JsonMerge {
                    servers_key: "mcpServers",
                    server_name: "mcp-agent-mail",
                    server_value: standard_http_server_value(url, token),
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
                "headers": { "Authorization": format!("Bearer {token}") }
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
                "headers": { "Authorization": format!("Bearer {token}") }
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

/// Execute a single config write action, returning the outcome.
pub fn write_config_atomic(action: &ConfigAction) -> Result<ActionOutcome, SetupError> {
    let parent = action.file_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let existing = std::fs::read_to_string(&action.file_path).ok();

    let new_content = match &action.content {
        ConfigContent::JsonMerge {
            servers_key,
            server_name,
            server_value,
        } => merge_mcp_server(
            existing.as_deref(),
            servers_key,
            server_name,
            server_value.clone(),
        )?,
        ConfigContent::JsonFull(val) => serde_json::to_string_pretty(val)? + "\n",
        ConfigContent::HooksMerge {
            project_slug,
            agent_name,
        } => merge_claude_hooks(existing.as_deref(), project_slug, agent_name)?,
    };

    // Check if unchanged
    if existing.as_deref() == Some(&new_content) {
        return Ok(ActionOutcome::Unchanged);
    }

    let was_existing = existing.is_some();

    // Backup existing file
    if action.backup && was_existing {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let file_name = action
            .file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let backup = parent.join(format!(".{file_name}.{ts}.bak"));
        std::fs::copy(&action.file_path, &backup)?;
    }

    // Atomic write: write to temp, then rename
    let temp = action.file_path.with_extension("tmp");
    std::fs::write(&temp, &new_content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(action.permissions))?;
    }

    std::fs::rename(&temp, &action.file_path)?;

    if was_existing {
        Ok(ActionOutcome::Updated)
    } else {
        Ok(ActionOutcome::Created)
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Run the full setup flow.
#[must_use]
pub fn run_setup(params: &SetupParams) -> Vec<SetupResult> {
    let platforms = params
        .agents
        .clone()
        .unwrap_or_else(|| AgentPlatform::ALL.to_vec());

    let mut results = Vec::new();

    for platform in &platforms {
        let actions = platform.config_actions(params);
        let mut action_results = Vec::new();

        for action in &actions {
            let outcome = if params.dry_run {
                ActionOutcome::Skipped
            } else {
                match write_config_atomic(action) {
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

    // Ensure .gitignore has entries for secret files
    if !params.dry_run {
        let gitignore = params.project_dir.join(".gitignore");
        // .env contains the bearer token — always gitignore it
        let mut entries = vec![".env"];
        // .claude/settings.local.json only exists for Claude
        if platforms.contains(&AgentPlatform::Claude) {
            entries.push(".claude/settings.local.json");
        }
        let _ = ensure_gitignore_entries(&gitignore, &entries);
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

/// Status of a single config file.
#[derive(Debug, Serialize)]
pub struct ConfigFileStatus {
    pub path: String,
    pub exists: bool,
    pub has_server_entry: bool,
    pub url_matches: bool,
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
        let actions = platform.config_actions(params);
        let mut file_statuses = Vec::new();

        for action in &actions {
            // Skip hooks for status check
            if matches!(action.content, ConfigContent::HooksMerge { .. }) {
                continue;
            }
            let exists = action.file_path.exists();
            let (has_server, url_matches) = if exists {
                check_config_file(&action.file_path, &url)
            } else {
                (false, false)
            };

            file_statuses.push(ConfigFileStatus {
                path: action.file_path.display().to_string(),
                exists,
                has_server_entry: has_server,
                url_matches,
            });
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

/// Check whether a config file contains our server entry and the URL matches.
fn check_config_file(path: &Path, expected_url: &str) -> (bool, bool) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (false, false);
    };
    let Ok(doc) = serde_json::from_str::<Value>(&content) else {
        return (false, false);
    };

    // Search for "mcp-agent-mail" at any nesting level in known keys
    for key in &["mcpServers", "mcp", "servers"] {
        if let Some(servers) = doc.get(key).and_then(|v| v.as_object())
            && let Some(entry) = servers.get("mcp-agent-mail")
        {
            let url_match = entry
                .get("url")
                .or_else(|| entry.get("httpUrl"))
                .and_then(|v| v.as_str())
                .is_some_and(|u| u == expected_url);
            return (true, url_match);
        }
    }

    (false, false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn generate_token_is_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_unique_across_calls() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
    }

    #[test]
    fn resolve_token_explicit_wins() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let t = resolve_token(Some("my-explicit-token"), tmp.path());
        assert_eq!(t, "my-explicit-token");
    }

    #[test]
    fn resolve_token_generates_when_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-env");
        let t = resolve_token(None, &missing);
        assert_eq!(t.len(), 64);
    }

    #[test]
    fn resolve_token_reads_env_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "FOO=bar\nHTTP_BEARER_TOKEN=from-file\nBAZ=qux\n",
        )
        .unwrap();
        let t = resolve_token(None, tmp.path());
        assert_eq!(t, "from-file");
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
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c").join("config.json");
        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: deep.clone(),
            description: "test".into(),
            content: ConfigContent::JsonFull(json!({"hello": "world"})),
            permissions: 0o644,
            backup: false,
        };
        let outcome = write_config_atomic(&action).unwrap();
        assert_eq!(outcome, ActionOutcome::Created);
        assert!(deep.exists());
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&deep).unwrap()).unwrap();
        assert_eq!(content["hello"], "world");
    }

    #[test]
    fn write_config_atomic_backs_up_existing() {
        let tmp = tempfile::tempdir().unwrap();
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
        let outcome = write_config_atomic(&action).unwrap();
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
    fn write_config_atomic_unchanged_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");

        // Write initial via merge
        let initial =
            merge_mcp_server(None, "mcpServers", "test", json!({"url": "http://a"})).unwrap();
        std::fs::write(&path, &initial).unwrap();

        let action = ConfigAction {
            platform: AgentPlatform::Cursor,
            file_path: path,
            description: "test".into(),
            content: ConfigContent::JsonMerge {
                servers_key: "mcpServers",
                server_name: "test",
                server_value: json!({"url": "http://a"}),
            },
            permissions: 0o644,
            backup: false,
        };
        let outcome = write_config_atomic(&action).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        save_token_to_env_file(&env_path, "my-token-123").unwrap();
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("HTTP_BEARER_TOKEN=my-token-123"));
    }

    #[test]
    fn save_token_to_env_file_updates() {
        let tmp = tempfile::tempdir().unwrap();
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

    #[test]
    fn gitignore_append_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
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
    fn agent_platform_all_has_nine() {
        assert_eq!(AgentPlatform::ALL.len(), 9);
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
}
