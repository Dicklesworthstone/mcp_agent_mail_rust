use std::collections::HashMap;

use crate::Config;
use crate::atc_user_surfaces::{ATC_CANARY_REPORT_DIR_ENV, ATC_CANARY_REPORT_PATH_ENV};
use crate::config::{
    AtcExecutorMode, atc_policy_bundle_path, atc_population_limit, atc_population_recency_secs,
    dotenv_value, full_env_value, load_dotenv_file, process_env_value, update_envfile,
    user_env_value,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FlagKind {
    Bool,
    Enum(&'static [&'static str]),
    /// Non-negative integer knob; floors/ceilings are stated in `doc`.
    Integer,
    /// Finite floating-point knob; accepted range is stated in `doc`.
    Float,
    /// Filesystem path; empty/whitespace means unset.
    Path,
    /// Free-form text (for example a comma-separated pattern list).
    Text,
}

impl FlagKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Enum(_) => "enum",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Path => "path",
            Self::Text => "text",
        }
    }

    #[must_use]
    pub const fn allowed_values(self) -> &'static [&'static str] {
        match self {
            Self::Bool => &["true", "false"],
            Self::Enum(values) => values,
            Self::Integer | Self::Float | Self::Path | Self::Text => &[],
        }
    }
}

/// Display value for an optional path-style knob that is not set.
pub const UNSET_VALUE: &str = "(unset)";

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

fn current_atc_enabled(config: &Config) -> String {
    bool_string(config.atc_enabled)
}

fn current_atc_executor_mode(_config: &Config) -> String {
    AtcExecutorMode::from_env().as_str().to_string()
}

fn current_atc_probe_interval_secs(config: &Config) -> String {
    config.atc_probe_interval_secs.to_string()
}

fn current_atc_advisory_cooldown_secs(config: &Config) -> String {
    config.atc_advisory_cooldown_secs.to_string()
}

fn current_atc_summary_interval_secs(config: &Config) -> String {
    config.atc_summary_interval_secs.to_string()
}

fn current_atc_safe_mode_recovery_count(config: &Config) -> String {
    config.atc_safe_mode_recovery_count.to_string()
}

fn current_atc_eprocess_threshold(config: &Config) -> String {
    config.atc_eprocess_threshold.to_string()
}

fn current_atc_cusum_threshold(config: &Config) -> String {
    config.atc_cusum_threshold.to_string()
}

fn current_atc_cusum_delta(config: &Config) -> String {
    config.atc_cusum_delta.to_string()
}

fn current_atc_ledger_capacity(config: &Config) -> String {
    config.atc_ledger_capacity.to_string()
}

fn current_atc_suspicion_k(config: &Config) -> String {
    config.atc_suspicion_k.to_string()
}

fn current_atc_experience_max_rows(config: &Config) -> String {
    config.atc_experience_max_rows.to_string()
}

fn current_atc_retention_sweep_interval_secs(config: &Config) -> String {
    config.atc_retention_sweep_interval_secs.to_string()
}

fn current_atc_population_recency_secs(_config: &Config) -> String {
    atc_population_recency_secs().to_string()
}

fn current_atc_population_limit(_config: &Config) -> String {
    atc_population_limit().to_string()
}

fn current_atc_policy_bundle_path(_config: &Config) -> String {
    atc_policy_bundle_path().unwrap_or_else(|| UNSET_VALUE.to_string())
}

fn optional_path_env(env_var: &str) -> String {
    full_env_value(env_var)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| UNSET_VALUE.to_string())
}

fn current_atc_canary_report_path(_config: &Config) -> String {
    optional_path_env(ATC_CANARY_REPORT_PATH_ENV)
}

fn current_atc_canary_report_dir(_config: &Config) -> String {
    optional_path_env(ATC_CANARY_REPORT_DIR_ENV)
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

fn current_coalescer_adaptive_flush_enabled(config: &Config) -> String {
    resolve_env_bool(config, "AM_COALESCER_ADAPTIVE_FLUSH_ENABLED", false)
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
        doc: "Experience-ledger persistence mode: off (no atc_experiences rows), shadow (trace-log what would be written), live (append durable rows to the ATC sidecar). Gates only the learning ledger; mail and reservation effects are governed by ATC_EXECUTOR_MODE.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc", "server", "robot"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some(
            "Effective value may be forced to off by ATC_LEARNING_DISABLED. Durable rows additionally require ATC_EXECUTOR_MODE=canary|live and a file-backed database.",
        ),
        resolve_value: current_atc_write_mode,
        resolve_source: effective_atc_write_mode_source,
    },
    // ---- Air Traffic Control (AM_ATC_*) — GH#290 -------------------------
    // Every `AM_ATC_*` variable read anywhere in the workspace MUST have an
    // entry here (enforced by `every_atc_env_var_in_source_is_registered`).
    FlagDefinition {
        name: "ATC_ENABLED",
        env_var: "AM_ATC_ENABLED",
        kind: FlagKind::Bool,
        default_value: "true",
        doc: "Master switch for Air Traffic Control. When false the ATC engine ignores every hook and the operator loop never starts: no liveness reviews, probes, advisories, reservation releases, population hydration, or experience rows. Liveness surfaces (am robot atc, TUI) then fall back to the database last_active_ts written by ordinary tool calls (\"passive liveness only\"); crashed agents' reservations are only reclaimed by TTL expiry, not by ATC.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "server", "robot", "reservations"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some(
            "Also forced to false by ATC_LEARNING_DISABLED. Prefer ATC_EXECUTOR_MODE=shadow (the default) to keep observation while stopping mail.",
        ),
        resolve_value: current_atc_enabled,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_ENABLED"),
    },
    FlagDefinition {
        name: "ATC_EXECUTOR_MODE",
        env_var: AtcExecutorMode::ENV_VAR,
        kind: FlagKind::Enum(AtcExecutorMode::VALUES),
        default_value: "shadow",
        doc: "How ATC decisions become side effects. shadow (default): observe and decide, emit nothing durable (counted as atc.shadow.would_insert). dry_run: same, but suppressed effects appear in the operator snapshot. canary: send activity-check/acknowledgment-request mail as real messages, never force-release reservations. live: execute everything, including reservation releases. Unknown values fall back to shadow.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc", "server", "messaging", "reservations"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some(
            "dry-run and dryrun are accepted aliases for dry_run. canary/live plus ATC_WRITE_MODE=live is what makes the experience ledger durable.",
        ),
        resolve_value: current_atc_executor_mode,
        resolve_source: |config| process_or_config_source(config, AtcExecutorMode::ENV_VAR),
    },
    FlagDefinition {
        name: "ATC_PROBE_INTERVAL_SECS",
        env_var: "AM_ATC_PROBE_INTERVAL_SECS",
        kind: FlagKind::Integer,
        default_value: "120",
        doc: "Operator tick interval and per-agent liveness-probe cadence, in seconds. Floor 5; the operator loop additionally never ticks faster than every 250 ms.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_probe_interval_secs,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_PROBE_INTERVAL_SECS"),
    },
    FlagDefinition {
        name: "ATC_ADVISORY_COOLDOWN_SECS",
        env_var: "AM_ATC_ADVISORY_COOLDOWN_SECS",
        kind: FlagKind::Integer,
        default_value: "300",
        doc: "Minimum seconds between advisories (activity checks, deadlock notices) to the same agent. Floor 10.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "messaging"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_advisory_cooldown_secs,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_ADVISORY_COOLDOWN_SECS"),
    },
    FlagDefinition {
        name: "ATC_SUMMARY_INTERVAL_SECS",
        env_var: "AM_ATC_SUMMARY_INTERVAL_SECS",
        kind: FlagKind::Integer,
        default_value: "300",
        doc: "Seconds between ATC summary lines in the operator console/log. Floor 10.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "tui"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_summary_interval_secs,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_SUMMARY_INTERVAL_SECS"),
    },
    FlagDefinition {
        name: "ATC_SAFE_MODE_RECOVERY_COUNT",
        env_var: "AM_ATC_SAFE_MODE_RECOVERY_COUNT",
        kind: FlagKind::Integer,
        default_value: "20",
        doc: "Consecutive correct liveness predictions required before ATC leaves safe mode (in safe mode it observes but takes no proactive action). Floor 1.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_safe_mode_recovery_count,
        resolve_source: |config| {
            process_or_config_source(config, "AM_ATC_SAFE_MODE_RECOVERY_COUNT")
        },
    },
    FlagDefinition {
        name: "ATC_EPROCESS_THRESHOLD",
        env_var: "AM_ATC_EPROCESS_THRESHOLD",
        kind: FlagKind::Float,
        default_value: "20",
        doc: "E-process (test martingale) alert threshold for calibration drift; crossing it enters safe mode. 20 corresponds to roughly a 5% significance level. Must be finite and > 0.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_eprocess_threshold,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_EPROCESS_THRESHOLD"),
    },
    FlagDefinition {
        name: "ATC_CUSUM_THRESHOLD",
        env_var: "AM_ATC_CUSUM_THRESHOLD",
        kind: FlagKind::Float,
        default_value: "5",
        doc: "CUSUM change-point detection threshold on prediction error. Must be finite and > 0.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_cusum_threshold,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_CUSUM_THRESHOLD"),
    },
    FlagDefinition {
        name: "ATC_CUSUM_DELTA",
        env_var: "AM_ATC_CUSUM_DELTA",
        kind: FlagKind::Float,
        default_value: "0.1",
        doc: "Minimum shift magnitude the CUSUM detector is tuned to catch. Must be finite and > 0.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_cusum_delta,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_CUSUM_DELTA"),
    },
    FlagDefinition {
        name: "ATC_LEDGER_CAPACITY",
        env_var: "AM_ATC_LEDGER_CAPACITY",
        kind: FlagKind::Integer,
        default_value: "1000",
        doc: "In-memory evidence ledger ring-buffer capacity (entries) backing decision transparency cards. Floor 10.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "tui"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_ledger_capacity,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_LEDGER_CAPACITY"),
    },
    FlagDefinition {
        name: "ATC_SUSPICION_K",
        env_var: "AM_ATC_SUSPICION_K",
        kind: FlagKind::Float,
        default_value: "3",
        doc: "Rhythm-based liveness suspicion factor: an agent becomes suspect once its silence exceeds its expected inter-activity gap by k standard deviations. Lower values probe sooner. Must be finite and > 0.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_suspicion_k,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_SUSPICION_K"),
    },
    FlagDefinition {
        name: "ATC_EXPERIENCE_MAX_ROWS",
        env_var: "AM_ATC_EXPERIENCE_MAX_ROWS",
        kind: FlagKind::Integer,
        default_value: "50000",
        doc: "Hard ceiling on raw atc_experiences rows in the ATC sidecar. Above it the retention sweep rolls up and evicts the oldest terminal rows, then force-rotates open rows if still over. 0 disables the ceiling.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "db"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some("Only matters when ATC_WRITE_MODE=live is producing rows."),
        resolve_value: current_atc_experience_max_rows,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_EXPERIENCE_MAX_ROWS"),
    },
    FlagDefinition {
        name: "ATC_RETENTION_SWEEP_INTERVAL_SECS",
        env_var: "AM_ATC_RETENTION_SWEEP_INTERVAL_SECS",
        kind: FlagKind::Integer,
        default_value: "900",
        doc: "Cadence, in seconds, of the background sweep that enforces ATC_EXPERIENCE_MAX_ROWS. 0 disables the sweep (the ceiling is then never enforced).",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "db"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_retention_sweep_interval_secs,
        resolve_source: |config| {
            process_or_config_source(config, "AM_ATC_RETENTION_SWEEP_INTERVAL_SECS")
        },
    },
    FlagDefinition {
        name: "ATC_POPULATION_RECENCY_SECS",
        env_var: "AM_ATC_POPULATION_RECENCY_SECS",
        kind: FlagKind::Integer,
        default_value: "604800",
        doc: "Recency window, in seconds, for hydrating agents from the database into ATC on cold start and periodic sync; agents whose last_active_ts is older are not loaded (they would only evaluate as Dead and burst effects). Default 7 days. 0 hydrates nobody; negative or unparsable values use the default.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some(
            "Lowering this is the supported mitigation for cold-start effect-queue saturation on mailboxes with many recently-active identities (GH#258).",
        ),
        resolve_value: current_atc_population_recency_secs,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_POPULATION_RECENCY_SECS"),
    },
    FlagDefinition {
        name: "ATC_POPULATION_LIMIT",
        env_var: "AM_ATC_POPULATION_LIMIT",
        kind: FlagKind::Integer,
        default_value: "4096",
        doc: "Maximum agents materialized by one ATC population sync (most recently active first). Values are clamped to 1..=65536; 0 or unparsable values use the default.",
        stability: FlagStability::Stable,
        subsystem: "atc",
        affected_subsystems: &["atc", "server"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_population_limit,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_POPULATION_LIMIT"),
    },
    FlagDefinition {
        name: "ATC_POLICY_BUNDLE_PATH",
        env_var: "AM_ATC_POLICY_BUNDLE_PATH",
        kind: FlagKind::Path,
        default_value: UNSET_VALUE,
        doc: "Path to an ATC liveness policy bundle JSON to load instead of the compiled-in baseline policy (see am atc simulate for producing candidates). Unset or empty uses the baseline.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: current_atc_policy_bundle_path,
        resolve_source: |config| process_or_config_source(config, "AM_ATC_POLICY_BUNDLE_PATH"),
    },
    FlagDefinition {
        name: "ATC_CANARY_REPORT_PATH",
        env_var: ATC_CANARY_REPORT_PATH_ENV,
        kind: FlagKind::Path,
        default_value: UNSET_VALUE,
        doc: "Exact path of an ATC canary perf-gate report JSON for am robot atc / the TUI to summarize. Unset reads <STORAGE_ROOT>/atc_perf_gate/latest_canary_report.json.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc", "robot", "tui"],
        dynamic_toggle: false,
        restart_required: false,
        notes: Some("Read on each robot/TUI render; no restart needed."),
        resolve_value: current_atc_canary_report_path,
        resolve_source: |config| process_or_config_source(config, ATC_CANARY_REPORT_PATH_ENV),
    },
    FlagDefinition {
        name: "ATC_CANARY_REPORT_DIR",
        env_var: ATC_CANARY_REPORT_DIR_ENV,
        kind: FlagKind::Path,
        default_value: UNSET_VALUE,
        doc: "Directory containing latest_canary_report.json for am robot atc / the TUI. ATC_CANARY_REPORT_PATH takes precedence; unset reads <STORAGE_ROOT>/atc_perf_gate/.",
        stability: FlagStability::Experimental,
        subsystem: "atc",
        affected_subsystems: &["atc", "robot", "tui"],
        dynamic_toggle: false,
        restart_required: false,
        notes: Some("Read on each robot/TUI render; no restart needed."),
        resolve_value: current_atc_canary_report_dir,
        resolve_source: |config| process_or_config_source(config, ATC_CANARY_REPORT_DIR_ENV),
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
        name: "COALESCER_ADAPTIVE_FLUSH_ENABLED",
        env_var: "AM_COALESCER_ADAPTIVE_FLUSH_ENABLED",
        kind: FlagKind::Bool,
        default_value: "false",
        doc: "Use adaptive archive commit-coalescer flush windows instead of shadow-only recommendations.",
        stability: FlagStability::Experimental,
        subsystem: "storage",
        affected_subsystems: &["storage", "archive", "robot"],
        dynamic_toggle: false,
        restart_required: true,
        notes: Some(
            "False keeps the controller in shadow mode; workers still record target/effective windows in per-repo stats.",
        ),
        resolve_value: current_coalescer_adaptive_flush_enabled,
        resolve_source: |config| {
            process_or_config_source(config, "AM_COALESCER_ADAPTIVE_FLUSH_ENABLED")
        },
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
        name: "RETENTION_REPORT_INTERVAL_SECONDS",
        env_var: "RETENTION_REPORT_INTERVAL_SECONDS",
        kind: FlagKind::Integer,
        default_value: "3600",
        doc: "Retention/quota worker scan interval in seconds; the worker floors it at 60.",
        stability: FlagStability::Stable,
        subsystem: "retention",
        affected_subsystems: &["retention"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: |config| config.retention_report_interval_seconds.to_string(),
        resolve_source: |config| {
            process_or_config_source(config, "RETENTION_REPORT_INTERVAL_SECONDS")
        },
    },
    FlagDefinition {
        name: "RETENTION_MAX_AGE_DAYS",
        env_var: "RETENTION_MAX_AGE_DAYS",
        kind: FlagKind::Integer,
        default_value: "180",
        doc: "Age threshold, in days, above which messages are counted by the read-only retention report.",
        stability: FlagStability::Stable,
        subsystem: "retention",
        affected_subsystems: &["retention"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: |config| config.retention_max_age_days.to_string(),
        resolve_source: |config| process_or_config_source(config, "RETENTION_MAX_AGE_DAYS"),
    },
    FlagDefinition {
        name: "RETENTION_IGNORE_PROJECT_PATTERNS",
        env_var: "RETENTION_IGNORE_PROJECT_PATTERNS",
        kind: FlagKind::Text,
        default_value: "demo,test*,testproj*,testproject,backendproj*,frontendproj*",
        doc: "Comma-separated project slug glob patterns skipped by retention reports.",
        stability: FlagStability::Stable,
        subsystem: "retention",
        affected_subsystems: &["retention"],
        dynamic_toggle: false,
        restart_required: true,
        notes: None,
        resolve_value: |config| config.retention_ignore_project_patterns.join(","),
        resolve_source: |config| {
            process_or_config_source(config, "RETENTION_IGNORE_PROJECT_PATTERNS")
        },
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

    let mut refreshed = Config::from_env();
    refreshed
        .console_persist_path
        .clone_from(&config.console_persist_path);
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

    /// Workspace root (`crates/mcp-agent-mail-core` -> repo root).
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_path_buf()
    }

    fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Every `AM_ATC_*` literal that appears anywhere in the workspace sources.
    fn atc_env_vars_in_sources() -> std::collections::BTreeMap<String, std::path::PathBuf> {
        let mut files = Vec::new();
        collect_rust_sources(&workspace_root().join("crates"), &mut files);
        assert!(
            files.len() > 50,
            "expected to scan the whole workspace, found only {} .rs files",
            files.len()
        );
        let mut found = std::collections::BTreeMap::new();
        for file in files {
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            let mut rest = source.as_str();
            while let Some(start) = rest.find("AM_ATC_") {
                let tail = &rest[start..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(tail.len());
                let name = &tail[..end];
                // `AM_ATC_` alone (prefix mentions in prose) is not a variable.
                if name.len() > "AM_ATC_".len() {
                    found
                        .entry(name.to_string())
                        .or_insert_with(|| file.clone());
                }
                rest = &tail[end..];
            }
        }
        found
    }

    fn atc_registry_env_vars() -> std::collections::BTreeSet<&'static str> {
        flag_registry()
            .iter()
            .map(|flag| flag.env_var)
            .filter(|env_var| env_var.starts_with("AM_ATC_"))
            .collect()
    }

    /// GH#290: the AM_ATC_* surface must be a single documented source of
    /// truth. Any `AM_ATC_*` variable read (or even mentioned) in code must be
    /// registered here, so a new knob cannot ship undocumented again.
    #[test]
    fn every_atc_env_var_in_source_is_registered() {
        let registered = atc_registry_env_vars();
        let in_sources = atc_env_vars_in_sources();
        assert!(
            in_sources.len() >= 19,
            "expected the known 19-variable AM_ATC_* surface, scanned {in_sources:?}"
        );
        let unregistered: Vec<String> = in_sources
            .iter()
            .filter(|(name, _)| !registered.contains(name.as_str()))
            .map(|(name, file)| format!("{name} (first seen in {})", file.display()))
            .collect();
        assert!(
            unregistered.is_empty(),
            "AM_ATC_* variables used in code but missing from FLAG_REGISTRY: {unregistered:#?}"
        );
        // And the reverse: nothing registered that the code never reads.
        let phantom: Vec<&str> = registered
            .iter()
            .copied()
            .filter(|name| !in_sources.contains_key(*name))
            .collect();
        assert!(
            phantom.is_empty(),
            "FLAG_REGISTRY lists AM_ATC_* variables no code reads: {phantom:?}"
        );
    }

    /// GH#290: README, the flag registry doc, and the operator runbook must
    /// carry every AM_ATC_* variable with its registered default.
    #[test]
    fn every_atc_env_var_is_documented_with_its_default() {
        let root = workspace_root();
        let readme = std::fs::read_to_string(root.join("README.md")).expect("README.md");
        let registry_doc =
            std::fs::read_to_string(root.join("docs/FLAGS_REGISTRY.md")).expect("FLAGS_REGISTRY");
        let runbook = std::fs::read_to_string(root.join("docs/OPERATOR_RUNBOOK.md"))
            .expect("OPERATOR_RUNBOOK");

        let mut problems = Vec::new();
        for flag in flag_registry() {
            let registry_row = format!(
                "| `{}` | `{}` | `{}` |",
                flag.name, flag.env_var, flag.default_value
            );
            if !registry_doc.contains(&registry_row) {
                problems.push(format!("docs/FLAGS_REGISTRY.md lacks row {registry_row}"));
            }
            if flag.subsystem != "atc" {
                continue;
            }
            let readme_row = format!("| `{}` | `{}` |", flag.env_var, flag.default_value);
            if !readme.contains(&readme_row) {
                problems.push(format!("README.md ATC table lacks row {readme_row}"));
            }
            let runbook_cell = format!("| `{}` |", flag.env_var);
            if !runbook.contains(&runbook_cell) {
                problems.push(format!(
                    "docs/OPERATOR_RUNBOOK.md ATC env table lacks {}",
                    flag.env_var
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "flag docs drifted:\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn typed_defaults_parse_as_their_kind() {
        for flag in flag_registry() {
            match flag.kind {
                FlagKind::Integer => {
                    flag.default_value.parse::<i64>().unwrap_or_else(|e| {
                        panic!(
                            "{}: integer default {:?}: {e}",
                            flag.name, flag.default_value
                        )
                    });
                }
                FlagKind::Float => {
                    let value = flag.default_value.parse::<f64>().unwrap_or_else(|e| {
                        panic!("{}: float default {:?}: {e}", flag.name, flag.default_value)
                    });
                    assert!(value.is_finite(), "{}: non-finite default", flag.name);
                }
                FlagKind::Bool => assert!(
                    matches!(flag.default_value, "true" | "false"),
                    "{}: bool default {:?}",
                    flag.name,
                    flag.default_value
                ),
                FlagKind::Enum(values) => assert!(
                    values.contains(&flag.default_value),
                    "{}: enum default {:?} not in {:?}",
                    flag.name,
                    flag.default_value,
                    values
                ),
                FlagKind::Path => assert_eq!(flag.default_value, UNSET_VALUE, "{}", flag.name),
                FlagKind::Text => {}
            }
        }
    }

    #[test]
    fn atc_snapshots_reflect_env_overrides_with_clamps() {
        crate::config::with_process_env_overrides_for_test(
            &[
                ("AM_ATC_EXECUTOR_MODE", "Canary"),
                ("AM_ATC_POPULATION_LIMIT", "999999"),
                ("AM_ATC_POPULATION_RECENCY_SECS", "-5"),
                ("AM_ATC_POLICY_BUNDLE_PATH", "  /tmp/bundle.json "),
                ("AM_ATC_CANARY_REPORT_DIR", "   "),
                ("AM_ATC_PROBE_INTERVAL_SECS", "1"),
            ],
            || {
                let config = Config::from_env();
                let value = |name: &str| {
                    let flag = find_flag(name).expect(name);
                    let snapshot = flag_snapshot(&config, flag);
                    (snapshot.current_value, snapshot.source)
                };
                assert_eq!(
                    value("AM_ATC_EXECUTOR_MODE"),
                    ("canary".into(), "env".into())
                );
                assert_eq!(
                    value("ATC_POPULATION_LIMIT"),
                    ("65536".into(), "env".into())
                );
                // Negative recency is rejected and reports the default value,
                // but the source still names the (ignored) env override.
                assert_eq!(
                    value("AM_ATC_POPULATION_RECENCY_SECS"),
                    ("604800".into(), "env".into())
                );
                assert_eq!(
                    value("ATC_POLICY_BUNDLE_PATH"),
                    ("/tmp/bundle.json".into(), "env".into())
                );
                assert_eq!(
                    value("ATC_CANARY_REPORT_DIR").0,
                    UNSET_VALUE,
                    "whitespace-only path must read as unset"
                );
                assert_eq!(
                    value("ATC_PROBE_INTERVAL_SECS").0,
                    "5",
                    "floor of 5s applies"
                );
            },
        );
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

        let config = Config {
            console_persist_path: path,
            atc_write_mode: AtcWriteMode::Off,
            ..Config::default()
        };

        let flag = find_flag("ATC_WRITE_MODE").expect("flag");
        assert_eq!(flag.current_source(&config), FlagSource::ConfigFile);
    }

    #[test]
    fn worktrees_source_tracks_git_identity_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.env");
        std::fs::write(&path, "GIT_IDENTITY_ENABLED=true\n").expect("write env");

        let config = Config {
            console_persist_path: path,
            worktrees_enabled: true,
            ..Config::default()
        };

        let flag = find_flag("WORKTREES_ENABLED").expect("flag");
        assert_eq!(flag.current_source(&config), FlagSource::ConfigFile);
    }

    #[test]
    fn toggle_dynamic_bool_flag_writes_console_envfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.env");

        let config = Config {
            console_persist_path: path.clone(),
            ..Config::default()
        };

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
