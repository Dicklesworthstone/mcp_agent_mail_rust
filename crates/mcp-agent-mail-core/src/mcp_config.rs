//! MCP client configuration path detection.
//!
//! This module centralizes known MCP config file locations across supported
//! coding-agent tools so installer/doctor flows can reason about takeover
//! and migration in one place.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Supported MCP client tools that may hold config files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpConfigTool {
    Claude,
    Codex,
    Cursor,
    Gemini,
    GithubCopilot,
    Windsurf,
    Cline,
    OpenCode,
    FactoryDroid,
}

impl McpConfigTool {
    /// Canonical slug for machine-oriented output.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::GithubCopilot => "github-copilot",
            Self::Windsurf => "windsurf",
            Self::Cline => "cline",
            Self::OpenCode => "opencode",
            Self::FactoryDroid => "factory",
        }
    }
}

/// One MCP config candidate path and whether it currently exists on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfigLocation {
    pub tool: McpConfigTool,
    pub config_path: PathBuf,
    pub exists: bool,
}

/// Inputs for MCP config path detection.
#[derive(Debug, Clone, Default)]
pub struct McpConfigDetectParams {
    /// Home directory override. Falls back to `dirs::home_dir()`.
    pub home_dir: Option<PathBuf>,
    /// Project directory override. Falls back to process CWD.
    pub project_dir: Option<PathBuf>,
    /// `%APPDATA%`-style directory for Windows layouts.
    pub app_data_dir: Option<PathBuf>,
}

/// Detect known MCP config locations across supported coding-agent tools.
///
/// Returned entries are deduplicated by `(tool, path)` and sorted
/// deterministically by tool slug then path.
#[must_use]
pub fn detect_mcp_config_locations(params: &McpConfigDetectParams) -> Vec<McpConfigLocation> {
    let home_dir = params.home_dir.clone().or_else(dirs::home_dir);
    let project_dir = params
        .project_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut candidates: Vec<(McpConfigTool, PathBuf)> = Vec::new();
    let mut seen: HashSet<(McpConfigTool, PathBuf)> = HashSet::new();

    if let Some(home) = home_dir.as_ref() {
        add_home_candidates(&mut candidates, &mut seen, home);
    }

    if let Some(app_data) = params.app_data_dir.as_ref() {
        add_app_data_candidates(&mut candidates, &mut seen, app_data);
    }

    add_project_candidates(&mut candidates, &mut seen, &project_dir);

    let mut locations = candidates
        .into_iter()
        .map(|(tool, config_path)| McpConfigLocation {
            tool,
            exists: config_path.exists(),
            config_path,
        })
        .collect::<Vec<_>>();
    locations.sort_by(|a, b| {
        a.tool
            .slug()
            .cmp(b.tool.slug())
            .then_with(|| a.config_path.cmp(&b.config_path))
    });
    locations
}

/// Detect known MCP config locations using ambient runtime paths.
#[must_use]
pub fn detect_mcp_config_locations_default() -> Vec<McpConfigLocation> {
    let params = McpConfigDetectParams {
        app_data_dir: std::env::var_os("APPDATA").map(PathBuf::from),
        ..McpConfigDetectParams::default()
    };
    detect_mcp_config_locations(&params)
}

const TARGET_SERVER_NAME: &str = "mcp-agent-mail";
const SERVER_CONTAINER_KEYS: &[&str] = &["mcpServers", "servers", "mcp", "mcp_servers"];

/// Result of updating MCP config text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigTextUpdate {
    pub updated_text: String,
    pub changed: bool,
    pub target_found: bool,
    pub used_json5_fallback: bool,
}

/// Result of updating an MCP config file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigFileUpdate {
    pub config_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub changed: bool,
    pub target_found: bool,
    pub used_json5_fallback: bool,
}

/// Errors from MCP config parsing or update.
#[derive(Debug, thiserror::Error)]
pub enum McpConfigUpdateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config must be a JSON object at top level")]
    TopLevelNotObject,

    #[error("config parse failed as JSON ({json_error}) and JSON5 ({json5_error})")]
    ParseFailed {
        json_error: String,
        json5_error: String,
    },

    #[error("failed to serialize updated config: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("failed to decode serialized config as UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("updated config did not validate as strict JSON: {0}")]
    Validation(String),
}

/// Update a config text blob by rewriting only the `mcp-agent-mail` server entry.
///
/// - Preserves sibling MCP servers untouched
/// - Preserves `env` object from existing `mcp-agent-mail` entry
/// - Rewrites `command` to an absolute Rust binary path
/// - Translates Python invocation args to Rust equivalents
/// - Accepts JSON and JSON5-like input (comments/trailing commas/BOM)
pub fn update_mcp_config_text(
    text: &str,
    rust_binary_path: &Path,
) -> Result<McpConfigTextUpdate, McpConfigUpdateError> {
    let (body, had_bom) = strip_utf8_bom(text);
    let style = detect_render_style(body, had_bom);
    let (mut doc, used_json5_fallback) = parse_json_or_json5(body)?;
    let (target_found, changed) = update_target_entry(&mut doc, rust_binary_path)?;
    if !changed {
        return Ok(McpConfigTextUpdate {
            updated_text: text.to_string(),
            changed: false,
            target_found,
            used_json5_fallback,
        });
    }

    let updated_text = render_json_with_style(&doc, &style)?;
    validate_strict_json(&updated_text)?;

    Ok(McpConfigTextUpdate {
        updated_text,
        changed: true,
        target_found,
        used_json5_fallback,
    })
}

/// Update an MCP config file in-place with backup.
///
/// The file is only rewritten if the target entry changed.
pub fn update_mcp_config_file(
    config_path: &Path,
    rust_binary_path: &Path,
) -> Result<McpConfigFileUpdate, McpConfigUpdateError> {
    let original = std::fs::read_to_string(config_path)?;
    let update = update_mcp_config_text(&original, rust_binary_path)?;
    if !update.changed {
        return Ok(McpConfigFileUpdate {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            changed: false,
            target_found: update.target_found,
            used_json5_fallback: update.used_json5_fallback,
        });
    }

    let backup_path = backup_path_for(config_path);
    std::fs::copy(config_path, &backup_path)?;
    std::fs::write(config_path, &update.updated_text)?;
    validate_strict_json(&update.updated_text)?;

    Ok(McpConfigFileUpdate {
        config_path: config_path.to_path_buf(),
        backup_path: Some(backup_path),
        changed: true,
        target_found: update.target_found,
        used_json5_fallback: update.used_json5_fallback,
    })
}

#[derive(Debug, Clone)]
struct RenderStyle {
    indent: Vec<u8>,
    newline: &'static str,
    trailing_newline: bool,
    had_bom: bool,
}

fn strip_utf8_bom(text: &str) -> (&str, bool) {
    text.strip_prefix('\u{FEFF}')
        .map_or((text, false), |stripped| (stripped, true))
}

fn detect_render_style(text_without_bom: &str, had_bom: bool) -> RenderStyle {
    let newline = if text_without_bom.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let trailing_newline = text_without_bom.ends_with('\n');
    let indent = detect_indent(text_without_bom);
    RenderStyle {
        indent,
        newline,
        trailing_newline,
        had_bom,
    }
}

fn detect_indent(text: &str) -> Vec<u8> {
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('}') {
            continue;
        }
        let ws_len = line.len().saturating_sub(trimmed.len());
        if ws_len > 0 {
            return line.as_bytes()[..ws_len].to_vec();
        }
    }
    b"  ".to_vec()
}

fn parse_json_or_json5(text: &str) -> Result<(Value, bool), McpConfigUpdateError> {
    match serde_json::from_str::<Value>(text) {
        Ok(doc) => Ok((doc, false)),
        Err(json_error) => match json5::from_str::<Value>(text) {
            Ok(doc) => Ok((doc, true)),
            Err(json5_parse_error) => Err(McpConfigUpdateError::ParseFailed {
                json_error: json_error.to_string(),
                json5_error: json5_parse_error.to_string(),
            }),
        },
    }
}

fn update_target_entry(
    doc: &mut Value,
    rust_binary_path: &Path,
) -> Result<(bool, bool), McpConfigUpdateError> {
    let Some(root_obj) = doc.as_object_mut() else {
        return Err(McpConfigUpdateError::TopLevelNotObject);
    };

    for key in SERVER_CONTAINER_KEYS {
        let Some(servers) = root_obj.get_mut(*key) else {
            continue;
        };
        let Some(servers_obj) = servers.as_object_mut() else {
            continue;
        };
        let Some(existing_entry) = servers_obj.get_mut(TARGET_SERVER_NAME) else {
            continue;
        };

        let old = existing_entry.clone();
        let updated = build_updated_server_entry(&old, rust_binary_path);
        let changed = updated != old;
        *existing_entry = updated;
        return Ok((true, changed));
    }

    Ok((false, false))
}

fn build_updated_server_entry(existing: &Value, rust_binary_path: &Path) -> Value {
    let mut entry = existing
        .as_object()
        .cloned()
        .unwrap_or_else(Map::<String, Value>::new);
    let preserved_env = entry.get("env").filter(|value| value.is_object()).cloned();
    let old_args = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|values| value_array_to_strings(values))
        .unwrap_or_default();

    let rust_args = translate_python_args_to_rust(&old_args)
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>();
    entry.insert(
        "command".to_string(),
        Value::String(rust_binary_path.to_string_lossy().into_owned()),
    );
    entry.insert("args".to_string(), Value::Array(rust_args));
    if let Some(env) = preserved_env {
        entry.insert("env".to_string(), env);
    }
    Value::Object(entry)
}

fn value_array_to_strings(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn translate_python_args_to_rust(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "-m" => {
                skip_next = true;
            }
            "mcp_agent_mail" | "mcp_agent_mail.cli" | "serve-stdio" | "serve_stdio" => {}
            "serve-http" | "serve_http" => out.push("serve".to_string()),
            _ => out.push(arg.clone()),
        }
    }
    if requires_serve_subcommand(&out) && !out.iter().any(|arg| arg == "serve") {
        out.insert(0, "serve".to_string());
    }
    out
}

fn requires_serve_subcommand(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--host" | "--port" | "--path" | "--no-auth" | "--no-tui"
        ) || arg.starts_with("--host=")
            || arg.starts_with("--port=")
            || arg.starts_with("--path=")
    })
}

fn render_json_with_style(
    doc: &Value,
    style: &RenderStyle,
) -> Result<String, McpConfigUpdateError> {
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&style.indent);
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    doc.serialize(&mut serializer)?;
    let mut rendered = String::from_utf8(bytes)?;
    if style.newline == "\r\n" {
        rendered = rendered.replace('\n', "\r\n");
    }
    if style.trailing_newline {
        rendered.push_str(style.newline);
    }
    if style.had_bom {
        rendered.insert(0, '\u{FEFF}');
    }
    Ok(rendered)
}

fn validate_strict_json(text: &str) -> Result<(), McpConfigUpdateError> {
    let (body, _) = strip_utf8_bom(text);
    serde_json::from_str::<Value>(body)
        .map(|_| ())
        .map_err(|error| McpConfigUpdateError::Validation(error.to_string()))
}

fn backup_path_for(path: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp-config.json");
    path.with_file_name(format!("{file_name}.{stamp}.bak"))
}

fn push_candidate(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    tool: McpConfigTool,
    path: PathBuf,
) {
    if path.as_os_str().is_empty() {
        return;
    }
    let key = (tool, path.clone());
    if seen.insert(key) {
        out.push((tool, path));
    }
}

fn add_home_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    add_home_claude_candidates(out, seen, home);
    add_home_codex_candidates(out, seen, home);
    add_home_cursor_candidates(out, seen, home);
    add_home_gemini_candidates(out, seen, home);
    add_home_github_copilot_candidates(out, seen, home);
    add_home_other_tool_candidates(out, seen, home);
}

fn add_home_claude_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        home.join(".claude").join("settings.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        home.join(".claude").join("settings.local.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        home.join(".claude").join("claude_desktop_config.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        home.join(".config")
            .join("Claude")
            .join("claude_desktop_config.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        home.join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json"),
    );
}

fn add_home_codex_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Codex,
        home.join(".codex").join("config.toml"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Codex,
        home.join(".codex").join("config.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Codex,
        home.join(".config").join("codex").join("config.toml"),
    );
}

fn add_home_cursor_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Cursor,
        home.join(".cursor").join("mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Cursor,
        home.join(".cursor").join("mcp_config.json"),
    );
}

fn add_home_gemini_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Gemini,
        home.join(".gemini").join("settings.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Gemini,
        home.join(".gemini").join("mcp.json"),
    );
}

fn add_home_github_copilot_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::GithubCopilot,
        home.join(".config")
            .join("Code")
            .join("User")
            .join("settings.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::GithubCopilot,
        home.join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("settings.json"),
    );
}

fn add_home_other_tool_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    home: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Windsurf,
        home.join(".windsurf").join("mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Cline,
        home.join(".cline").join("mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::OpenCode,
        home.join(".opencode").join("opencode.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::FactoryDroid,
        home.join(".factory").join("mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::FactoryDroid,
        home.join(".factory").join("settings.json"),
    );
}

fn add_app_data_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    app_data: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        app_data.join("Claude").join("claude_desktop_config.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::GithubCopilot,
        app_data.join("Code").join("User").join("settings.json"),
    );
}

fn add_project_candidates(
    out: &mut Vec<(McpConfigTool, PathBuf)>,
    seen: &mut HashSet<(McpConfigTool, PathBuf)>,
    project_dir: &Path,
) {
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        project_dir.join(".claude").join("settings.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Claude,
        project_dir.join(".claude").join("settings.local.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Codex,
        project_dir.join(".codex").join("config.toml"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Codex,
        project_dir.join("codex.mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Cursor,
        project_dir.join("cursor.mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Gemini,
        project_dir.join("gemini.mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::GithubCopilot,
        project_dir.join(".vscode").join("mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Windsurf,
        project_dir.join("windsurf.mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::Cline,
        project_dir.join("cline.mcp.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::OpenCode,
        project_dir.join("opencode.json"),
    );
    push_candidate(
        out,
        seen,
        McpConfigTool::FactoryDroid,
        project_dir.join("factory.mcp.json"),
    );
}

// ---------------------------------------------------------------------------
// Fresh MCP config creation (for new installs without existing Python entry)
// ---------------------------------------------------------------------------

/// Parameters for creating a new `mcp-agent-mail` server entry.
#[derive(Debug, Clone)]
pub struct NewServerEntryParams {
    /// Absolute path to the Rust `mcp-agent-mail` binary.
    pub rust_binary_path: PathBuf,
    /// Optional bearer token. If `None`, the caller should generate one.
    pub bearer_token: Option<String>,
    /// Optional storage root override. If `None`, the env key is omitted.
    pub storage_root: Option<String>,
}

/// Build a fresh `mcp-agent-mail` server entry as a JSON value.
#[must_use]
pub fn build_new_server_entry(params: &NewServerEntryParams) -> Value {
    let mut env = Map::new();
    if let Some(ref token) = params.bearer_token {
        env.insert(
            "HTTP_BEARER_TOKEN".to_string(),
            Value::String(token.clone()),
        );
    }
    if let Some(ref root) = params.storage_root {
        env.insert("STORAGE_ROOT".to_string(), Value::String(root.clone()));
    }

    let mut entry = Map::new();
    entry.insert(
        "command".to_string(),
        Value::String(params.rust_binary_path.to_string_lossy().into_owned()),
    );
    entry.insert("args".to_string(), Value::Array(Vec::new()));
    if !env.is_empty() {
        entry.insert("env".to_string(), Value::Object(env));
    }
    Value::Object(entry)
}

/// Insert the `mcp-agent-mail` server entry into an existing config text.
///
/// If the config already contains the entry, returns unchanged.
/// If the server container key exists but has no `mcp-agent-mail`, adds it.
/// If neither `mcpServers` nor alternatives exist, creates `mcpServers`.
pub fn insert_server_entry_text(
    text: &str,
    params: &NewServerEntryParams,
) -> Result<McpConfigTextUpdate, McpConfigUpdateError> {
    let (body, had_bom) = strip_utf8_bom(text);
    let style = detect_render_style(body, had_bom);
    let (mut doc, used_json5_fallback) = parse_json_or_json5(body)?;

    let root_obj = doc
        .as_object_mut()
        .ok_or(McpConfigUpdateError::TopLevelNotObject)?;

    // Check if the entry already exists under any server container key.
    for key in SERVER_CONTAINER_KEYS {
        if let Some(servers) = root_obj.get(*key) {
            if let Some(servers_obj) = servers.as_object() {
                if servers_obj.contains_key(TARGET_SERVER_NAME) {
                    return Ok(McpConfigTextUpdate {
                        updated_text: text.to_string(),
                        changed: false,
                        target_found: true,
                        used_json5_fallback,
                    });
                }
            }
        }
    }

    // Find an existing container to insert into, or create `mcpServers`.
    let container_key = SERVER_CONTAINER_KEYS
        .iter()
        .find(|key| {
            root_obj
                .get(**key)
                .is_some_and(|value| value.is_object())
        })
        .copied()
        .unwrap_or("mcpServers");

    let entry = build_new_server_entry(params);

    let servers_obj = root_obj
        .entry(container_key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(McpConfigUpdateError::TopLevelNotObject)?;

    servers_obj.insert(TARGET_SERVER_NAME.to_string(), entry);

    let updated_text = render_json_with_style(&doc, &style)?;
    validate_strict_json(&updated_text)?;

    Ok(McpConfigTextUpdate {
        updated_text,
        changed: true,
        target_found: false,
        used_json5_fallback,
    })
}

/// Create a brand new MCP config file text with only the `mcp-agent-mail` entry.
#[must_use]
pub fn create_fresh_config_text(params: &NewServerEntryParams) -> String {
    let entry = build_new_server_entry(params);
    let mut servers = Map::new();
    servers.insert(TARGET_SERVER_NAME.to_string(), entry);
    let mut doc = Map::new();
    doc.insert("mcpServers".to_string(), Value::Object(servers));
    serde_json::to_string_pretty(&doc).expect("serialization of fresh config cannot fail") + "\n"
}

/// Result of setting up an MCP config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigSetupResult {
    pub config_path: PathBuf,
    /// The backup path if an existing file was modified.
    pub backup_path: Option<PathBuf>,
    /// Whether the file was created new (`true`) or an existing file was modified.
    pub created_new: bool,
    /// Whether any change was actually made (false if entry already existed).
    pub changed: bool,
}

/// Set up an MCP config file: create it if absent, or insert the entry if missing.
///
/// This is the main entry point for fresh-install MCP config setup.
pub fn setup_mcp_config_file(
    config_path: &Path,
    params: &NewServerEntryParams,
) -> Result<McpConfigSetupResult, McpConfigUpdateError> {
    if !config_path.exists() {
        // Create parent directories if needed.
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = create_fresh_config_text(params);
        std::fs::write(config_path, &text)?;
        return Ok(McpConfigSetupResult {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            created_new: true,
            changed: true,
        });
    }

    // File exists: read, parse, insert if missing.
    let original = std::fs::read_to_string(config_path)?;
    let update = insert_server_entry_text(&original, params)?;

    if !update.changed {
        return Ok(McpConfigSetupResult {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            created_new: false,
            changed: false,
        });
    }

    let backup = backup_path_for(config_path);
    std::fs::copy(config_path, &backup)?;
    std::fs::write(config_path, &update.updated_text)?;

    Ok(McpConfigSetupResult {
        config_path: config_path.to_path_buf(),
        backup_path: Some(backup),
        created_new: false,
        changed: true,
    })
}

/// Preferred config path for a given tool (the first candidate path).
///
/// Returns the primary config path for a tool regardless of whether it exists.
/// This is used during fresh installs to decide where to create a new config.
#[must_use]
pub fn preferred_config_path(tool: McpConfigTool, home: &Path) -> PathBuf {
    match tool {
        McpConfigTool::Claude => home.join(".claude").join("settings.json"),
        McpConfigTool::Codex => home.join(".codex").join("config.json"),
        McpConfigTool::Cursor => home.join(".cursor").join("mcp.json"),
        McpConfigTool::Gemini => home.join(".gemini").join("settings.json"),
        McpConfigTool::GithubCopilot => home
            .join(".config")
            .join("Code")
            .join("User")
            .join("settings.json"),
        McpConfigTool::Windsurf => home.join(".windsurf").join("mcp.json"),
        McpConfigTool::Cline => home.join(".cline").join("mcp.json"),
        McpConfigTool::OpenCode => home.join(".opencode").join("opencode.json"),
        McpConfigTool::FactoryDroid => home.join(".factory").join("mcp.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::path::Path;

    fn contains_location(
        locations: &[McpConfigLocation],
        tool: McpConfigTool,
        path: &Path,
    ) -> bool {
        locations
            .iter()
            .any(|entry| entry.tool == tool && entry.config_path == path)
    }

    #[test]
    fn detect_locations_include_required_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let appdata = tmp.path().join("AppData").join("Roaming");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&project).expect("create project");
        std::fs::create_dir_all(&appdata).expect("create appdata");

        let locations = detect_mcp_config_locations(&McpConfigDetectParams {
            home_dir: Some(home.clone()),
            project_dir: Some(project.clone()),
            app_data_dir: Some(appdata.clone()),
        });

        assert!(
            contains_location(
                &locations,
                McpConfigTool::Claude,
                &home
                    .join(".config")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            ),
            "expected Linux Claude Desktop config path"
        );
        assert!(
            contains_location(
                &locations,
                McpConfigTool::Claude,
                &home
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            ),
            "expected macOS Claude Desktop config path"
        );
        assert!(
            contains_location(
                &locations,
                McpConfigTool::Claude,
                &appdata.join("Claude").join("claude_desktop_config.json")
            ),
            "expected Windows APPDATA Claude Desktop config path"
        );
        assert!(
            contains_location(
                &locations,
                McpConfigTool::Codex,
                &home.join(".codex").join("config.toml")
            ),
            "expected codex config.toml path"
        );
        assert!(
            contains_location(
                &locations,
                McpConfigTool::Gemini,
                &home.join(".gemini").join("settings.json")
            ),
            "expected gemini settings path"
        );
        assert!(
            contains_location(
                &locations,
                McpConfigTool::GithubCopilot,
                &project.join(".vscode").join("mcp.json")
            ),
            "expected copilot workspace mcp path"
        );
    }

    #[test]
    fn detect_locations_tracks_exists_flag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).expect("create codex dir");
        std::fs::create_dir_all(project.join(".vscode")).expect("create vscode dir");
        std::fs::write(home.join(".codex").join("config.toml"), "ok").expect("write codex");
        std::fs::write(project.join(".vscode").join("mcp.json"), "{}").expect("write vscode mcp");

        let locations = detect_mcp_config_locations(&McpConfigDetectParams {
            home_dir: Some(home.clone()),
            project_dir: Some(project),
            app_data_dir: None,
        });

        let codex = locations
            .iter()
            .find(|entry| {
                entry.tool == McpConfigTool::Codex
                    && entry.config_path == home.join(".codex").join("config.toml")
            })
            .expect("codex location present");
        assert!(
            codex.exists,
            "existing codex config should be marked exists=true"
        );

        let gemini = locations
            .iter()
            .find(|entry| {
                entry.tool == McpConfigTool::Gemini
                    && entry.config_path == home.join(".gemini").join("settings.json")
            })
            .expect("gemini location present");
        assert!(
            !gemini.exists,
            "missing gemini config should be marked exists=false"
        );
    }

    #[test]
    fn detect_locations_deduplicate_same_tool_and_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let appdata = home.join(".config");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&appdata).expect("create appdata");

        let locations = detect_mcp_config_locations(&McpConfigDetectParams {
            home_dir: Some(home.clone()),
            project_dir: Some(tmp.path().join("project")),
            app_data_dir: Some(appdata),
        });

        let target = home
            .join(".config")
            .join("Claude")
            .join("claude_desktop_config.json");
        let count = locations
            .iter()
            .filter(|entry| entry.tool == McpConfigTool::Claude && entry.config_path == target)
            .count();
        assert_eq!(count, 1, "duplicate MCP config paths must be collapsed");
    }

    fn mcp_entry(doc: &Value) -> &Value {
        doc.get("mcpServers")
            .and_then(|servers| servers.get("mcp-agent-mail"))
            .expect("mcp-agent-mail entry present")
    }

    #[test]
    fn update_config_text_only_mutates_target_entry_and_preserves_env() {
        let rust_bin = Path::new("/opt/mcp-agent-mail/bin/mcp-agent-mail");
        let original = r#"{
  "mcpServers": {
    "other-server": {
      "command": "node",
      "args": ["dist/server.js"]
    },
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail", "serve-http", "--path", "/api/", "--port", "8765"],
      "env": {
        "HTTP_BEARER_TOKEN": "secret",
        "STORAGE_ROOT": "/tmp/archive"
      },
      "transport": "stdio"
    }
  },
  "theme": "dark"
}
"#;

        let update = update_mcp_config_text(original, rust_bin).expect("update succeeds");
        assert!(update.changed, "target entry should be rewritten");
        assert!(update.target_found, "target entry should be discovered");
        assert!(
            !update.used_json5_fallback,
            "strict JSON should not need JSON5 fallback"
        );

        let doc: Value = serde_json::from_str(&update.updated_text).expect("valid strict JSON");
        let target = mcp_entry(&doc);
        assert_eq!(
            target.get("command").and_then(Value::as_str),
            Some("/opt/mcp-agent-mail/bin/mcp-agent-mail")
        );
        assert_eq!(
            target.get("args"),
            Some(&json!(["serve", "--path", "/api/", "--port", "8765"]))
        );
        assert_eq!(
            target.get("env"),
            Some(&json!({
                "HTTP_BEARER_TOKEN": "secret",
                "STORAGE_ROOT": "/tmp/archive"
            }))
        );
        assert_eq!(
            target.get("transport").and_then(Value::as_str),
            Some("stdio"),
            "non-command fields should remain untouched"
        );
        assert_eq!(
            doc.get("mcpServers")
                .and_then(|servers| servers.get("other-server")),
            Some(&json!({
                "command": "node",
                "args": ["dist/server.js"]
            })),
            "sibling MCP servers must remain untouched"
        );
        assert_eq!(
            doc.get("theme").and_then(Value::as_str),
            Some("dark"),
            "non-MCP top-level keys must remain untouched"
        );
    }

    #[test]
    fn update_config_text_returns_unchanged_when_target_missing() {
        let rust_bin = Path::new("/usr/local/bin/mcp-agent-mail");
        let original = r#"{
  "mcpServers": {
    "other": {
      "command": "node",
      "args": ["index.js"]
    }
  }
}
"#;

        let update = update_mcp_config_text(original, rust_bin).expect("update succeeds");
        assert!(!update.changed, "no target entry means no mutation");
        assert!(
            !update.target_found,
            "target should not be reported as found"
        );
        assert_eq!(
            update.updated_text, original,
            "text should round-trip untouched when no changes occur"
        );
    }

    #[test]
    fn update_config_text_accepts_bom_comments_and_trailing_commas() {
        let rust_bin = Path::new("/home/user/.local/bin/mcp-agent-mail");
        let original = concat!(
            "\u{FEFF}{\n",
            "  // JSONC comment\n",
            "  \"mcpServers\": {\n",
            "    \"mcp-agent-mail\": {\n",
            "      \"command\": \"python\",\n",
            "      \"args\": [\"-m\", \"mcp_agent_mail\", \"serve-stdio\",],\n",
            "      \"env\": {\"STORAGE_ROOT\": \"/tmp/store\",},\n",
            "    },\n",
            "  },\n",
            "}\n"
        );

        let update = update_mcp_config_text(original, rust_bin).expect("update succeeds");
        assert!(update.changed, "target entry should be updated");
        assert!(update.target_found, "target entry should be found");
        assert!(
            update.used_json5_fallback,
            "JSONC/trailing commas should exercise JSON5 parser fallback"
        );
        assert!(
            update.updated_text.starts_with('\u{FEFF}'),
            "BOM should be preserved"
        );

        let strict_body = update
            .updated_text
            .strip_prefix('\u{FEFF}')
            .expect("BOM preserved");
        let doc: Value = serde_json::from_str(strict_body).expect("output must be strict JSON");
        let target = mcp_entry(&doc);
        assert_eq!(
            target.get("command").and_then(Value::as_str),
            Some("/home/user/.local/bin/mcp-agent-mail")
        );
        assert_eq!(target.get("args"), Some(&json!([])));
        assert_eq!(
            target.get("env"),
            Some(&json!({"STORAGE_ROOT": "/tmp/store"}))
        );
    }

    #[test]
    fn update_config_file_creates_backup_and_rewrites_when_changed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("mcp.json");
        let rust_bin = Path::new("/home/test/.local/bin/mcp-agent-mail");
        let original = r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail"]
    }
  }
}
"#;
        std::fs::write(&config_path, original).expect("write config");

        let update = update_mcp_config_file(&config_path, rust_bin).expect("file update succeeds");
        assert!(update.changed, "expected file rewrite");
        assert!(update.target_found, "target must be found");

        let backup = update.backup_path.expect("backup path");
        assert!(backup.exists(), "backup file must exist");
        let backup_content = std::fs::read_to_string(&backup).expect("read backup");
        assert_eq!(
            backup_content, original,
            "backup should contain original config bytes"
        );

        let rewritten = std::fs::read_to_string(&config_path).expect("read rewritten");
        let doc: Value = serde_json::from_str(&rewritten).expect("rewritten must be strict JSON");
        let target = mcp_entry(&doc);
        assert_eq!(
            target.get("command").and_then(Value::as_str),
            Some("/home/test/.local/bin/mcp-agent-mail")
        );
        assert_eq!(target.get("args"), Some(&json!([])));
    }

    #[test]
    fn update_config_file_noop_when_already_rust_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("mcp.json");
        let rust_bin = Path::new("/home/test/.local/bin/mcp-agent-mail");
        let original = r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "/home/test/.local/bin/mcp-agent-mail",
      "args": [],
      "env": {
        "HTTP_BEARER_TOKEN": "token"
      }
    }
  }
}
"#;
        std::fs::write(&config_path, original).expect("write config");

        let update = update_mcp_config_file(&config_path, rust_bin).expect("file update succeeds");
        assert!(!update.changed, "already-correct entry should be a no-op");
        assert!(update.target_found, "target should still be found");
        assert!(
            update.backup_path.is_none(),
            "no-op update should not create a backup"
        );

        let post = std::fs::read_to_string(&config_path).expect("read config");
        assert_eq!(
            post, original,
            "no-op update should not rewrite file contents"
        );
    }

    // -----------------------------------------------------------------------
    // Tests for fresh MCP config creation (br-28mgh.4.3)
    // -----------------------------------------------------------------------

    fn default_params() -> NewServerEntryParams {
        NewServerEntryParams {
            rust_binary_path: PathBuf::from("/home/user/.local/bin/mcp-agent-mail"),
            bearer_token: Some("abc123".to_string()),
            storage_root: None,
        }
    }

    #[test]
    fn build_new_server_entry_with_token_and_storage() {
        let params = NewServerEntryParams {
            rust_binary_path: PathBuf::from("/opt/bin/mcp-agent-mail"),
            bearer_token: Some("mytoken".to_string()),
            storage_root: Some("/data/archive".to_string()),
        };
        let entry = build_new_server_entry(&params);
        assert_eq!(
            entry.get("command").and_then(Value::as_str),
            Some("/opt/bin/mcp-agent-mail")
        );
        assert_eq!(entry.get("args"), Some(&json!([])));
        let env = entry.get("env").expect("env object");
        assert_eq!(
            env.get("HTTP_BEARER_TOKEN").and_then(Value::as_str),
            Some("mytoken")
        );
        assert_eq!(
            env.get("STORAGE_ROOT").and_then(Value::as_str),
            Some("/data/archive")
        );
    }

    #[test]
    fn build_new_server_entry_without_optional_fields() {
        let params = NewServerEntryParams {
            rust_binary_path: PathBuf::from("/usr/bin/mcp-agent-mail"),
            bearer_token: None,
            storage_root: None,
        };
        let entry = build_new_server_entry(&params);
        assert_eq!(
            entry.get("command").and_then(Value::as_str),
            Some("/usr/bin/mcp-agent-mail")
        );
        assert!(entry.get("env").is_none(), "no env when no token/storage");
    }

    #[test]
    fn create_fresh_config_produces_valid_json() {
        let text = create_fresh_config_text(&default_params());
        let doc: Value = serde_json::from_str(&text).expect("valid JSON");
        let servers = doc
            .get("mcpServers")
            .and_then(Value::as_object)
            .expect("mcpServers");
        assert!(
            servers.contains_key("mcp-agent-mail"),
            "entry must be present"
        );
        let entry = &servers["mcp-agent-mail"];
        assert_eq!(
            entry.get("command").and_then(Value::as_str),
            Some("/home/user/.local/bin/mcp-agent-mail")
        );
        assert!(text.ends_with('\n'), "trailing newline expected");
    }

    #[test]
    fn insert_entry_into_existing_config_without_target() {
        let original = r#"{
  "mcpServers": {
    "other-server": {
      "command": "node",
      "args": ["server.js"]
    }
  }
}
"#;
        let update = insert_server_entry_text(original, &default_params()).expect("insert works");
        assert!(update.changed, "should insert new entry");
        assert!(!update.target_found, "target did not exist before");

        let doc: Value = serde_json::from_str(&update.updated_text).expect("valid JSON");
        let servers = doc
            .get("mcpServers")
            .and_then(Value::as_object)
            .expect("mcpServers");
        assert!(
            servers.contains_key("mcp-agent-mail"),
            "new entry must be present"
        );
        assert!(
            servers.contains_key("other-server"),
            "existing entry must be preserved"
        );
    }

    #[test]
    fn insert_entry_noop_when_already_present() {
        let original = r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail"]
    }
  }
}
"#;
        let update = insert_server_entry_text(original, &default_params()).expect("insert works");
        assert!(!update.changed, "entry exists, no change");
        assert!(update.target_found, "entry was found");
        assert_eq!(update.updated_text, original);
    }

    #[test]
    fn insert_entry_creates_mcp_servers_key_when_absent() {
        let original = r#"{
  "theme": "dark"
}
"#;
        let update = insert_server_entry_text(original, &default_params()).expect("insert works");
        assert!(update.changed, "should add mcpServers + entry");

        let doc: Value = serde_json::from_str(&update.updated_text).expect("valid JSON");
        assert!(
            doc.get("mcpServers")
                .and_then(Value::as_object)
                .unwrap()
                .contains_key("mcp-agent-mail"),
            "entry must be inserted under new mcpServers"
        );
        assert_eq!(
            doc.get("theme").and_then(Value::as_str),
            Some("dark"),
            "existing keys preserved"
        );
    }

    #[test]
    fn insert_entry_uses_existing_servers_key_variant() {
        // Some tools use "servers" instead of "mcpServers"
        let original = r#"{
  "servers": {
    "other": { "command": "node", "args": [] }
  }
}
"#;
        let update = insert_server_entry_text(original, &default_params()).expect("insert works");
        assert!(update.changed);
        let doc: Value = serde_json::from_str(&update.updated_text).expect("valid JSON");
        assert!(
            doc.get("servers")
                .and_then(Value::as_object)
                .unwrap()
                .contains_key("mcp-agent-mail"),
            "should insert under existing 'servers' key"
        );
    }

    #[test]
    fn setup_mcp_config_file_creates_new_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = tmp.path().join("subdir").join("mcp.json");
        let result = setup_mcp_config_file(&config, &default_params()).expect("setup");
        assert!(result.created_new);
        assert!(result.changed);
        assert!(result.backup_path.is_none());
        assert!(config.exists());

        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        assert!(doc
            .get("mcpServers")
            .and_then(Value::as_object)
            .unwrap()
            .contains_key("mcp-agent-mail"));
    }

    #[test]
    fn setup_mcp_config_file_inserts_into_existing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = tmp.path().join("mcp.json");
        let original = r#"{
  "mcpServers": {
    "other": { "command": "node", "args": [] }
  }
}
"#;
        std::fs::write(&config, original).expect("write");
        let result = setup_mcp_config_file(&config, &default_params()).expect("setup");
        assert!(!result.created_new);
        assert!(result.changed);
        assert!(result.backup_path.is_some(), "backup created on modify");

        let backup = result.backup_path.unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).expect("read backup"),
            original,
            "backup has original content"
        );

        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        let servers = doc
            .get("mcpServers")
            .and_then(Value::as_object)
            .unwrap();
        assert!(servers.contains_key("mcp-agent-mail"));
        assert!(servers.contains_key("other"));
    }

    #[test]
    fn setup_mcp_config_file_noop_when_entry_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = tmp.path().join("mcp.json");
        let original = r#"{
  "mcpServers": {
    "mcp-agent-mail": {
      "command": "/home/user/.local/bin/mcp-agent-mail",
      "args": []
    }
  }
}
"#;
        std::fs::write(&config, original).expect("write");
        let result = setup_mcp_config_file(&config, &default_params()).expect("setup");
        assert!(!result.created_new);
        assert!(!result.changed);
        assert!(result.backup_path.is_none());
    }

    #[test]
    fn preferred_config_path_returns_expected_paths() {
        let home = Path::new("/home/user");
        assert_eq!(
            preferred_config_path(McpConfigTool::Claude, home),
            PathBuf::from("/home/user/.claude/settings.json")
        );
        assert_eq!(
            preferred_config_path(McpConfigTool::Cursor, home),
            PathBuf::from("/home/user/.cursor/mcp.json")
        );
        assert_eq!(
            preferred_config_path(McpConfigTool::Gemini, home),
            PathBuf::from("/home/user/.gemini/settings.json")
        );
    }
}
