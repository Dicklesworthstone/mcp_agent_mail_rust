use std::collections::HashMap;

use crate::Config;
use crate::config::{
    dotenv_value, load_dotenv_file, process_env_value, update_envfile, user_env_value,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FlagKind {
    Bool,
    Enum(&'static [&'static str]),
}

impl FlagKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Enum(_) => "enum",
        }
    }

    #[must_use]
    pub const fn allowed_values(self) -> &'static [&'static str] {
        match self {
            Self::Bool => &["true", "false"],
            Self::Enum(values) => values,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FlagStability {
    Stable,
    Experimental,
    Deprecated,
}

impl FlagStability {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Experimental => "experimental",
            Self::Deprecated => "deprecated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FlagSource {
    Env,
    ConfigFile,
    ProjectDotenv,
    Default,
}

impl FlagSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::ConfigFile => "config",
            Self::ProjectDotenv => ".env",
            Self::Default => "default",
        }
    }
}

impl std::fmt::Display for FlagSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

pub struct FlagDefinition {
    pub name: &'static str,
    pub env_var: &'static str,
    pub kind: FlagKind,
    pub default_value: &'static str,
    pub doc: &'static str,
    pub stability: FlagStability,
    pub subsystem: &'static str,
    pub affected_subsystems: &'static [&'static str],
    pub dynamic_toggle: bool,
    pub restart_required: bool,
    pub notes: Option<&'static str>,
    resolve_value: fn(&Config) -> String,
    resolve_source: fn(&Config) -> FlagSource,
}

impl FlagDefinition {
    #[must_use]
    pub fn current_value(&self, config: &Config) -> String {
        (self.resolve_value)(config)
    }

    #[must_use]
    pub fn current_source(&self, config: &Config) -> FlagSource {
        (self.resolve_source)(config)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FlagSnapshot {
    pub name: String,
    pub env_var: String,
    pub kind: String,
    pub allowed_values: Vec<String>,
    pub subsystem: String,
    pub affected_subsystems: Vec<String>,
    pub stability: String,
    pub dynamic_toggle: bool,
    pub restart_required: bool,
    pub default_value: String,
    pub current_value: String,
    pub source: String,
    pub doc: String,
    pub notes: Option<String>,
    pub config_path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FlagRegistryError {
    #[error("unknown flag '{0}'")]
    UnknownFlag(String),
    #[error("flag '{name}' is not a boolean toggle")]
    NotBoolean { name: String },
    #[error("flag '{name}' cannot be toggled at runtime; restart is required")]
    RestartRequired { name: String },
    #[error(
        "flag '{name}' is currently overridden by process env var {env_var}; clear that env var before writing config"
    )]
    ProcessEnvOverride { name: String, env_var: String },
    #[error("failed to update {path}: {source}")]
    Persist {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

const ATC_WRITE_MODE_VALUES: &[&str] = &["off", "shadow", "live"];

fn bool_string(value: bool) -> String {
    if value {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

fn trim_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn process_or_config_source(config: &Config, env_var: &str) -> FlagSource {
    if process_env_value(env_var).is_some() {
        return FlagSource::Env;
    }

    let persisted = load_dotenv_file(&config.console_persist_path);
    if persisted.contains_key(env_var) {
        return FlagSource::ConfigFile;
    }
    if user_env_value(env_var).is_some() {
        return FlagSource::ConfigFile;
    }
    if dotenv_value(env_var).is_some() {
        return FlagSource::ProjectDotenv;
    }
    FlagSource::Default
}

fn effective_atc_write_mode_source(config: &Config) -> FlagSource {
    let kill_switch_source = process_or_config_source(config, "ATC_LEARNING_DISABLED");
    if !matches!(kill_switch_source, FlagSource::Default) {
        return kill_switch_source;
    }
    process_or_config_source(config, "AM_ATC_WRITE_MODE")
}

fn effective_worktrees_source(config: &Config) -> FlagSource {
    let git_identity_source = process_or_config_source(config, "GIT_IDENTITY_ENABLED");
    if !matches!(git_identity_source, FlagSource::Default) {
        return git_identity_source;
    }
    process_or_config_source(config, "WORKTREES_ENABLED")
}

fn resolve_env_bool(config: &Config, env_var: &str, default: bool) -> String {
    if let Some(value) = process_env_value(env_var) {
        return bool_string(trim_bool(&value));
    }
    let persisted = load_dotenv_file(&config.console_persist_path);
    if let Some(value) = persisted.get(env_var) {
        return bool_string(trim_bool(value));
    }
    if let Some(value) = user_env_value(env_var) {
        return bool_string(trim_bool(&value));
    }
    if let Some(value) = dotenv_value(env_var) {
        return bool_string(trim_bool(&value));
    }
    bool_string(default)
}

fn current_worktrees_enabled(config: &Config) -> String {
    bool_string(config.worktrees_enabled)
}

fn current_http_allow_localhost_unauthenticated(config: &Config) -> String {
    bool_string(config.http_allow_localhost_unauthenticated)
}

fn current_tui_enabled(config: &Config) -> String {
    bool_string(config.tui_enabled)
}

fn current_tui_effects(config: &Config) -> String {
    bool_string(config.tui_effects)
}

fn current_atc_write_mode(config: &Config) -> String {
    config.atc_write_mode.to_string()
}

fn current_atc_learning_disabled(config: &Config) -> String {
    resolve_env_bool(config, "ATC_LEARNING_DISABLED", false)
}

fn current_llm_enabled(config: &Config) -> String {
    bool_string(config.llm_enabled)
}

fn current_notifications_enabled(config: &Config) -> String {
    bool_string(config.notifications_enabled)
}

fn current_tool_filter_enabled(config: &Config) -> String {
    bool_string(config.tool_filter.enabled)
}

fn current_backpressure_shedding_enabled(config: &Config) -> String {
    bool_string(config.backpressure_shedding_enabled)
}

fn current_ack_ttl_enabled(config: &Config) -> String {
    bool_string(config.ack_ttl_enabled)
}

fn current_ack_escalation_enabled(config: &Config) -> String {
    bool_string(config.ack_escalation_enabled)
}

fn current_retention_report_enabled(config: &Config) -> String {
    bool_string(config.retention_report_enabled)
}

fn current_quota_enabled(config: &Config) -> String {
    bool_string(config.quota_enabled)
}

pub const FLAG_REGISTRY: &[FlagDefinition] = &[
    FlagDefinition {
        name: "ACK_ESCALATION_ENABLED",
        env_var: "ACK_ESCALATION_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Escalate overdue acknowledgements via the configured claim and escalation policy.",
        stability: FlagStability::Experimental,
        subsystem: "messaging",
        affected_subsystems: &["messaging", "reservations"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Server processes read this at startup."),
        resolve_value: current_ack_escalation_enabled,
        resolve_source: |config| process_or_config_source(config, "ACK_ESCALATION_ENABLED"),
    },
    FlagDefinition {
        name: "ACK_TTL_ENABLED",
        env_var: "ACK_TTL_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Emit overdue-ack warnings and scans for message acknowledgements.",
        stability: FlagStability::Stable,
        subsystem: "messaging",
        affected_subsystems: &["messaging", "analytics"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Server processes read this at startup."),
        resolve_value: current_ack_ttl_enabled,
        resolve_source: |config| process_or_config_source(config, "ACK_TTL_ENABLED"),
    },
    FlagDefinition {
        name: "ATC_LEARNING_DISABLED",
        env_var: "ATC_LEARNING_DISABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Hard kill-switch that forces ATC learning writes off regardless of write mode.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "server", "robot"],
        dynamic_toggle: true,
        restart_required: false,
        notes: Some("Takes precedence over ATC_WRITE_MODE."),
        resolve_value: current_atc_learning_disabled,
        resolve_source: |config| process_or_config_source(config, "ATC_LEARNING_DISABLED"),
    },
    FlagDefinition {
        name: "ATC_WRITE_MODE",
        env_var: "AM_ATC_WRITE_MODE",
        kind: FlagKind::Enum(ATC_WRITE_MODE_VALUES),
        default_value: "off",
        doc: "Controls ATC experience persistence mode: off, shadow, or live.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc", "server", "robot"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Effective value may be forced to off by ATC_LEARNING_DISABLED."),
        resolve_value: current_atc_write_mode,
        resolve_source: effective_atc_write_mode_source,
    },
    FlagDefinition {
        name: "BACKPRESSURE_SHEDDING_ENABLED",
        env_var: "BACKPRESSURE_SHEDDING_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Allow the server to shed low-priority work when health signals degrade.",
        stability: FlagStability::Experimental,
        subsystem: "server",
        affected_subsystems: &["server", "tools"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Server processes read this at startup."),
        resolve_value: current_backpressure_shedding_enabled,
        resolve_source: |config| process_or_config_source(config, "BACKPRESSURE_SHEDDING_ENABLED"),
    },
    FlagDefinition {
        name: "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED",
        env_var: "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Permit unauthenticated localhost HTTP access for local development only.",
        stability: FlagStability::Experimental,
        subsystem: "http",
        affected_subsystems: &["http", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Changing this only affects newly started HTTP servers."),
        resolve_value: current_http_allow_localhost_unauthenticated,
        resolve_source: |config| {
            process_or_config_source(config, "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED")
        },
    },
    FlagDefinition {
        name: "LLM_ENABLED",
        env_var: "LLM_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable LLM-backed features such as thread summarization and AI-assisted views.",
        stability: FlagStability::Experimental,
        subsystem: "llm",
        affected_subsystems: &["llm", "tools", "search"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Requires an explicit model configuration to be useful."),
        resolve_value: current_llm_enabled,
        resolve_source: |config| process_or_config_source(config, "LLM_ENABLED"),
    },
    FlagDefinition {
        name: "NOTIFICATIONS_ENABLED",
        env_var: "NOTIFICATIONS_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable filesystem notification signals for agent inbox changes.",
        stability: FlagStability::Stable,
        subsystem: "notifications",
        affected_subsystems: &["notifications", "storage"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Changing this only affects newly started workers."),
        resolve_value: current_notifications_enabled,
        resolve_source: |config| process_or_config_source(config, "NOTIFICATIONS_ENABLED"),
    },
    FlagDefinition {
        name: "QUOTA_ENABLED",
        env_var: "QUOTA_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable attachment and inbox quota enforcement.",
        stability: FlagStability::Experimental,
        subsystem: "quota",
        affected_subsystems: &["messaging", "storage"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Server processes read this at startup."),
        resolve_value: current_quota_enabled,
        resolve_source: |config| process_or_config_source(config, "QUOTA_ENABLED"),
    },
    FlagDefinition {
        name: "RETENTION_REPORT_ENABLED",
        env_var: "RETENTION_REPORT_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable periodic retention and compaction reports.",
        stability: FlagStability::Stable,
        subsystem: "retention",
        affected_subsystems: &["retention", "analytics"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Server processes read this at startup."),
        resolve_value: current_retention_report_enabled,
        resolve_source: |config| process_or_config_source(config, "RETENTION_REPORT_ENABLED"),
    },
    FlagDefinition {
        name: "TOOLS_FILTER_ENABLED",
        env_var: "TOOLS_FILTER_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable tool-filter profiles that reduce exposed tool surface area.",
        stability: FlagStability::Experimental,
        subsystem: "tool-filter",
        affected_subsystems: &["tools", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Profiles and tool lists still come from the wider config surface."),
        resolve_value: current_tool_filter_enabled,
        resolve_source: |config| process_or_config_source(config, "TOOLS_FILTER_ENABLED"),
    },
    FlagDefinition {
        name: "TUI_EFFECTS",
        env_var: "AM_TUI_EFFECTS",
        kind: FlagKind::Bool,
        default_value: "true",
        doc: "Enable ambient text and render effects in the TUI.",
        stability: FlagStability::Stable,
        subsystem: "tui",
        affected_subsystems: &["tui"],
        dynamic_toggle: true,
        restart_required: false,
        notes: Some("Persisted in the TUI config envfile."),
        resolve_value: current_tui_effects,
        resolve_source: |config| process_or_config_source(config, "AM_TUI_EFFECTS"),
    },
    FlagDefinition {
        name: "TUI_ENABLED",
        env_var: "TUI_ENABLED",
        kind: FlagKind::Bool,
        default_value: "true",
        doc: "Start the interactive TUI alongside the server.",
        stability: FlagStability::Stable,
        subsystem: "tui",
        affected_subsystems: &["tui", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Only affects new process starts."),
        resolve_value: current_tui_enabled,
        resolve_source: |config| process_or_config_source(config, "TUI_ENABLED"),
    },
    FlagDefinition {
        name: "WORKTREES_ENABLED",
        env_var: "WORKTREES_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Enable build-slot and Product Bus features that rely on worktree identity.",
        stability: FlagStability::Stable,
        subsystem: "worktrees",
        affected_subsystems: &["products", "build-slots", "identity"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Also implied by GIT_IDENTITY_ENABLED."),
        resolve_value: current_worktrees_enabled,
        resolve_source: effective_worktrees_source,
    },
];

#[must_use]
pub const fn flag_registry() -> &'static [FlagDefinition] {
    FLAG_REGISTRY
}

#[must_use]
pub fn find_flag(name: &str) -> Option<&'static FlagDefinition> {
    FLAG_REGISTRY.iter().find(|flag| {
        flag.name.eq_ignore_ascii_case(name) || flag.env_var.eq_ignore_ascii_case(name)
    })
}

#[must_use]
pub fn flag_snapshots(config: &Config) -> Vec<FlagSnapshot> {
    let mut flags = FLAG_REGISTRY
        .iter()
        .map(|flag| flag_snapshot(config, flag))
        .collect::<Vec<_>>();
    flags.sort_by(|left, right| {
        left.subsystem
            .cmp(&right.subsystem)
            .then_with(|| left.name.cmp(&right.name))
    });
    flags
}

#[must_use]
pub fn flag_snapshot(config: &Config, flag: &FlagDefinition) -> FlagSnapshot {
    FlagSnapshot {
        name: flag.name.to_string(),
        env_var: flag.env_var.to_string(),
        kind: flag.kind.label().to_string(),
        allowed_values: flag
            .kind
            .allowed_values()
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        subsystem: flag.subsystem.to_string(),
        affected_subsystems: flag
            .affected_subsystems
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        stability: flag.stability.label().to_string(),
        dynamic_toggle: flag.dynamic_toggle,
        restart_required: flag.restart_required,
        default_value: flag.default_value.to_string(),
        current_value: flag.current_value(config),
        source: flag.current_source(config).to_string(),
        doc: flag.doc.to_string(),
        notes: flag.notes.map(ToString::to_string),
        config_path: config.console_persist_path.display().to_string(),
    }
}

pub fn toggle_bool_flag(
    config: &Config,
    name: &str,
    enabled: bool,
) -> Result<FlagSnapshot, FlagRegistryError> {
    let flag = find_flag(name).ok_or_else(|| FlagRegistryError::UnknownFlag(name.to_string()))?;

    if !matches!(flag.kind, FlagKind::Bool) {
        return Err(FlagRegistryError::NotBoolean {
            name: flag.name.to_string(),
        });
    }
    if !flag.dynamic_toggle || flag.restart_required {
        return Err(FlagRegistryError::RestartRequired {
            name: flag.name.to_string(),
        });
    }
    if process_env_value(flag.env_var).is_some() {
        return Err(FlagRegistryError::ProcessEnvOverride {
            name: flag.name.to_string(),
            env_var: flag.env_var.to_string(),
        });
    }

    let mut updates = HashMap::new();
    updates.insert(flag.env_var, bool_string(enabled));
    update_envfile(&config.console_persist_path, &updates).map_err(|source| {
        FlagRegistryError::Persist {
            path: config.console_persist_path.display().to_string(),
            source,
        }
    })?;

    let refreshed = Config::from_env();
    Ok(flag_snapshot(&refreshed, flag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AtcWriteMode;

    #[test]
    fn registry_names_and_env_vars_are_unique() {
        let mut names = std::collections::HashSet::new();
        let mut env_vars = std::collections::HashSet::new();

        for flag in flag_registry() {
            assert!(names.insert(flag.name), "duplicate flag name {}", flag.name);
            assert!(
                env_vars.insert(flag.env_var),
                "duplicate flag env var {}",
                flag.env_var
            );
        }
    }

    #[test]
    fn registry_defaults_match_config_defaults() {
        let config = Config::default();

        for flag in flag_registry() {
            assert_eq!(
                flag.current_value(&config),
                flag.default_value,
                "default drift for {}",
                flag.name
            );
        }
    }

    #[test]
    fn atc_write_mode_source_prefers_kill_switch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.env");
        std::fs::write(
            &path,
            "ATC_LEARNING_DISABLED=true\nAM_ATC_WRITE_MODE=live\n",
        )
        .expect("write env");

        let mut config = Config::default();
        config.console_persist_path = path;
        config.atc_write_mode = AtcWriteMode::Off;

        let flag = find_flag("ATC_WRITE_MODE").expect("flag");
        assert_eq!(flag.current_source(&config), FlagSource::ConfigFile);
    }

    #[test]
    fn worktrees_source_tracks_git_identity_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.env");
        std::fs::write(&path, "GIT_IDENTITY_ENABLED=true\n").expect("write env");

        let mut config = Config::default();
        config.console_persist_path = path;
        config.worktrees_enabled = true;

        let flag = find_flag("WORKTREES_ENABLED").expect("flag");
        assert_eq!(flag.current_source(&config), FlagSource::ConfigFile);
    }

    #[test]
    fn toggle_dynamic_bool_flag_writes_console_envfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.env");

        let mut config = Config::default();
        config.console_persist_path = path.clone();

        let snapshot = toggle_bool_flag(&config, "ATC_LEARNING_DISABLED", true).expect("toggle");
        assert_eq!(snapshot.current_value, "true");
        assert_eq!(snapshot.source, "config");

        let written = std::fs::read_to_string(&path).expect("read env");
        assert!(written.contains("ATC_LEARNING_DISABLED=true"));
    }

    #[test]
    fn toggle_rejects_static_flag() {
        let config = Config::default();
        let err = toggle_bool_flag(&config, "TUI_ENABLED", false).expect_err("should fail");
        assert!(matches!(err, FlagRegistryError::RestartRequired { .. }));
    }
}
