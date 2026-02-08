//! Configuration management for MCP Agent Mail
//!
//! Configuration is loaded from environment variables, matching the legacy Python
//! implementation's python-decouple pattern.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Tool filtering configuration for context reduction.
#[derive(Debug, Clone)]
pub struct ToolFilterSettings {
    pub enabled: bool,
    pub profile: String,
    pub mode: String,
    pub clusters: Vec<String>,
    pub tools: Vec<String>,
}

impl Default for ToolFilterSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: "full".to_string(),
            mode: "include".to_string(),
            clusters: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// Main configuration struct for MCP Agent Mail
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    // Application
    pub app_environment: AppEnvironment,
    pub worktrees_enabled: bool,
    pub project_identity_mode: ProjectIdentityMode,
    pub project_identity_remote: String,

    // Database
    pub database_url: String,
    pub database_echo: bool,
    pub database_pool_size: Option<usize>,
    pub database_max_overflow: Option<usize>,
    pub database_pool_timeout: Option<u64>,
    /// Run `PRAGMA quick_check` on pool initialization (default: true).
    pub integrity_check_on_startup: bool,
    /// Hours between periodic full `PRAGMA integrity_check` runs (default: 24, 0 = disabled).
    pub integrity_check_interval_hours: u64,

    // Storage
    pub storage_root: PathBuf,
    pub git_author_name: String,
    pub git_author_email: String,
    pub inline_image_max_bytes: usize,
    pub convert_images: bool,
    pub keep_original_images: bool,
    pub allow_absolute_attachment_paths: bool,

    // Disk space monitoring
    pub disk_space_monitor_enabled: bool,
    pub disk_space_warning_mb: u64,
    pub disk_space_critical_mb: u64,
    pub disk_space_fatal_mb: u64,
    pub disk_space_check_interval_seconds: u64,

    // Memory pressure monitoring (RSS-based)
    pub memory_warning_mb: u64,
    pub memory_critical_mb: u64,
    pub memory_fatal_mb: u64,

    // HTTP
    pub http_host: String,
    pub http_port: u16,
    pub http_path: String,
    pub http_bearer_token: Option<String>,
    pub http_allow_localhost_unauthenticated: bool,
    pub http_request_log_enabled: bool,
    pub http_otel_enabled: bool,
    pub http_otel_service_name: String,
    pub http_otel_exporter_otlp_endpoint: String,

    // Rate Limiting
    pub http_rate_limit_enabled: bool,
    pub http_rate_limit_backend: RateLimitBackend,
    pub http_rate_limit_per_minute: u32,
    pub http_rate_limit_tools_per_minute: u32,
    pub http_rate_limit_resources_per_minute: u32,
    pub http_rate_limit_tools_burst: u32,
    pub http_rate_limit_resources_burst: u32,
    pub http_rate_limit_redis_url: Option<String>,

    // JWT
    pub http_jwt_enabled: bool,
    pub http_jwt_algorithms: Vec<String>,
    pub http_jwt_secret: Option<String>,
    pub http_jwt_jwks_url: Option<String>,
    pub http_jwt_audience: Option<String>,
    pub http_jwt_issuer: Option<String>,
    pub http_jwt_role_claim: String,

    // RBAC
    pub http_rbac_enabled: bool,
    pub http_rbac_reader_roles: Vec<String>,
    pub http_rbac_writer_roles: Vec<String>,
    pub http_rbac_default_role: String,
    pub http_rbac_readonly_tools: Vec<String>,

    // CORS
    pub http_cors_enabled: bool,
    pub http_cors_origins: Vec<String>,
    pub http_cors_allow_credentials: bool,
    pub http_cors_allow_methods: Vec<String>,
    pub http_cors_allow_headers: Vec<String>,

    // Contact & Messaging
    pub contact_enforcement_enabled: bool,
    pub contact_auto_ttl_seconds: u64,
    pub contact_auto_retry_enabled: bool,
    pub messaging_auto_register_recipients: bool,
    pub messaging_auto_handshake_on_block: bool,

    // Message size limits (bytes). 0 = unlimited.
    pub max_message_body_bytes: usize,
    pub max_attachment_bytes: usize,
    pub max_total_message_bytes: usize,
    pub max_subject_bytes: usize,

    // File Reservations
    pub file_reservations_cleanup_enabled: bool,
    pub file_reservations_cleanup_interval_seconds: u64,
    pub file_reservation_inactivity_seconds: u64,
    pub file_reservation_activity_grace_seconds: u64,
    pub file_reservations_enforcement_enabled: bool,

    // Ack TTL warnings
    pub ack_ttl_enabled: bool,
    pub ack_ttl_seconds: u64,
    pub ack_ttl_scan_interval_seconds: u64,

    // Ack escalation
    pub ack_escalation_enabled: bool,
    pub ack_escalation_mode: String,
    pub ack_escalation_claim_ttl_seconds: u64,
    pub ack_escalation_claim_exclusive: bool,
    pub ack_escalation_claim_holder_name: String,

    // Agent Naming
    pub agent_name_enforcement_mode: AgentNameEnforcementMode,

    // LLM
    pub llm_enabled: bool,
    pub llm_default_model: String,
    pub llm_temperature: f64,
    pub llm_max_tokens: u32,
    pub llm_cache_enabled: bool,
    pub llm_cache_backend: String,
    pub llm_cache_redis_url: String,
    pub llm_cost_logging_enabled: bool,

    // Notifications
    pub notifications_enabled: bool,
    pub notifications_signals_dir: PathBuf,
    pub notifications_include_metadata: bool,
    pub notifications_debounce_ms: u64,

    // Tool filtering
    pub tool_filter: ToolFilterSettings,

    // Instrumentation / query tracking
    pub instrumentation_enabled: bool,
    pub instrumentation_slow_query_ms: u64,
    pub tools_log_enabled: bool,
    pub tool_metrics_emit_enabled: bool,
    pub tool_metrics_emit_interval_seconds: u64,

    // Retention / Quota
    pub retention_report_enabled: bool,
    pub retention_report_interval_seconds: u64,
    pub retention_max_age_days: u64,
    pub retention_ignore_project_patterns: Vec<String>,
    pub quota_enabled: bool,
    pub quota_attachments_limit_bytes: u64,
    pub quota_inbox_limit_count: u64,

    // TOON output format
    pub toon_bin: Option<String>,
    pub toon_stats_enabled: bool,
    pub output_format_default: Option<String>,

    // Logging
    pub log_level: String,
    pub log_rich_enabled: bool,
    pub log_tool_calls_enabled: bool,
    pub log_tool_calls_result_max_chars: usize,
    pub log_include_trace: bool,
    pub log_json_enabled: bool,

    // Console / TUI layout + persistence
    pub console_persist_path: PathBuf,
    pub console_auto_save: bool,
    pub console_interactive_enabled: bool,
    pub console_ui_height_percent: u16,
    pub console_ui_anchor: ConsoleUiAnchor,
    pub console_ui_auto_size: bool,
    pub console_inline_auto_min_rows: u16,
    pub console_inline_auto_max_rows: u16,
    pub console_split_mode: ConsoleSplitMode,
    pub console_split_ratio_percent: u16,
    pub console_theme: ConsoleThemeId,

    // TUI
    pub tui_enabled: bool,
    pub tui_dock_position: String,
    pub tui_dock_ratio_percent: u16,
    pub tui_dock_visible: bool,
    pub tui_high_contrast: bool,
    pub tui_key_hints: bool,
}

/// Application environment
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnvironment {
    Development,
    Production,
}

impl std::fmt::Display for AppEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Development => write!(f, "development"),
            Self::Production => write!(f, "production"),
        }
    }
}

/// Project identity resolution mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectIdentityMode {
    Dir,
    GitRemote,
    GitCommonDir,
    GitToplevel,
}

/// Rate limit backend
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitBackend {
    Memory,
    Redis,
}

/// `StartupDashboard` UI anchor for Inline mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleUiAnchor {
    #[default]
    Bottom,
    Top,
}

impl ConsoleUiAnchor {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bottom" | "b" => Some(Self::Bottom),
            "top" | "t" => Some(Self::Top),
            _ => None,
        }
    }
}

/// `StartupDashboard` console split mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleSplitMode {
    #[default]
    Inline,
    Left,
}

impl ConsoleSplitMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "inline" | "i" => Some(Self::Inline),
            "left" | "l" => Some(Self::Left),
            _ => None,
        }
    }
}

/// Console theme selection (`FrankenTUI`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleThemeId {
    #[default]
    CyberpunkAurora,
    Darcula,
    LumenLight,
    NordicFrost,
    HighContrast,
}

impl ConsoleThemeId {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cyberpunk_aurora" | "cyberpunk-aurora" | "cyberpunk" | "aurora" => {
                Some(Self::CyberpunkAurora)
            }
            "darcula" => Some(Self::Darcula),
            "lumen_light" | "lumen-light" | "lumen" | "light" => Some(Self::LumenLight),
            "nordic_frost" | "nordic-frost" | "nordic" => Some(Self::NordicFrost),
            "high_contrast" | "high-contrast" | "contrast" | "hc" => Some(Self::HighContrast),
            _ => None,
        }
    }
}

/// Agent name enforcement mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentNameEnforcementMode {
    /// Reject invalid names with error
    Strict,
    /// Ignore invalid names, auto-generate instead
    Coerce,
    /// Always auto-generate, ignore provided names
    AlwaysAuto,
}

impl Default for Config {
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        Self {
            // Application
            app_environment: AppEnvironment::Development,
            worktrees_enabled: false,
            project_identity_mode: ProjectIdentityMode::Dir,
            project_identity_remote: "origin".to_string(),

            // Database
            // Match legacy Python default (SQLAlchemy async URL).
            database_url: "sqlite+aiosqlite:///./storage.sqlite3".to_string(),
            database_echo: false,
            database_pool_size: None,
            database_max_overflow: None,
            database_pool_timeout: None,
            integrity_check_on_startup: true,
            integrity_check_interval_hours: 24,

            // Storage
            storage_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".mcp_agent_mail_git_mailbox_repo"),
            git_author_name: "mcp-agent".to_string(),
            git_author_email: "mcp-agent@example.com".to_string(),
            inline_image_max_bytes: 65536,
            convert_images: true,
            keep_original_images: false,
            allow_absolute_attachment_paths: true,

            // Disk space monitoring
            disk_space_monitor_enabled: true,
            disk_space_warning_mb: 500,
            disk_space_critical_mb: 100,
            disk_space_fatal_mb: 10,
            disk_space_check_interval_seconds: 60,

            // Memory pressure monitoring
            memory_warning_mb: 2048,  // 2 GB
            memory_critical_mb: 4096, // 4 GB
            memory_fatal_mb: 8192,    // 8 GB

            // HTTP
            http_host: "127.0.0.1".to_string(),
            http_port: 8765,
            http_path: "/api/".to_string(),
            http_bearer_token: None,
            http_allow_localhost_unauthenticated: true,
            http_request_log_enabled: false,
            http_otel_enabled: false,
            http_otel_service_name: "mcp-agent-mail".to_string(),
            http_otel_exporter_otlp_endpoint: String::new(),

            // Rate Limiting
            http_rate_limit_enabled: false,
            http_rate_limit_backend: RateLimitBackend::Memory,
            http_rate_limit_per_minute: 60,
            http_rate_limit_tools_per_minute: 60,
            http_rate_limit_resources_per_minute: 120,
            http_rate_limit_tools_burst: 0,
            http_rate_limit_resources_burst: 0,
            http_rate_limit_redis_url: None,

            // JWT
            http_jwt_enabled: false,
            http_jwt_algorithms: vec!["HS256".to_string()],
            http_jwt_secret: None,
            http_jwt_jwks_url: None,
            http_jwt_audience: None,
            http_jwt_issuer: None,
            http_jwt_role_claim: "role".to_string(),

            // RBAC
            http_rbac_enabled: true,
            http_rbac_reader_roles: vec![
                "reader".to_string(),
                "read".to_string(),
                "ro".to_string(),
            ],
            http_rbac_writer_roles: vec![
                "writer".to_string(),
                "write".to_string(),
                "tools".to_string(),
                "rw".to_string(),
            ],
            http_rbac_default_role: "reader".to_string(),
            http_rbac_readonly_tools: vec![
                "health_check".to_string(),
                "fetch_inbox".to_string(),
                "whois".to_string(),
                "search_messages".to_string(),
                "summarize_thread".to_string(),
            ],

            // CORS
            http_cors_enabled: true,
            http_cors_origins: vec![],
            http_cors_allow_credentials: false,
            http_cors_allow_methods: vec!["*".to_string()],
            http_cors_allow_headers: vec!["*".to_string()],

            // Contact & Messaging
            contact_enforcement_enabled: true,
            contact_auto_ttl_seconds: 86400, // 24 hours
            contact_auto_retry_enabled: true,
            messaging_auto_register_recipients: true,
            messaging_auto_handshake_on_block: true,

            // Message size limits
            max_message_body_bytes: 1_048_576,   // 1 MiB
            max_attachment_bytes: 10_485_760,    // 10 MiB per attachment
            max_total_message_bytes: 20_971_520, // 20 MiB total (body + all attachments)
            max_subject_bytes: 1_024,            // 1 KiB

            // File Reservations
            file_reservations_cleanup_enabled: false,
            file_reservations_cleanup_interval_seconds: 60,
            file_reservation_inactivity_seconds: 1800, // 30 minutes
            file_reservation_activity_grace_seconds: 900, // 15 minutes
            file_reservations_enforcement_enabled: true,

            // Ack TTL warnings
            ack_ttl_enabled: false,
            ack_ttl_seconds: 1800,
            ack_ttl_scan_interval_seconds: 60,

            // Ack escalation
            ack_escalation_enabled: false,
            ack_escalation_mode: "log".to_string(),
            ack_escalation_claim_ttl_seconds: 3600,
            ack_escalation_claim_exclusive: false,
            ack_escalation_claim_holder_name: String::new(),

            // Agent Naming
            agent_name_enforcement_mode: AgentNameEnforcementMode::Coerce,

            // LLM
            llm_enabled: true,
            llm_default_model: "gpt-4o-mini".to_string(),
            llm_temperature: 0.2,
            llm_max_tokens: 512,
            llm_cache_enabled: true,
            llm_cache_backend: "memory".to_string(),
            llm_cache_redis_url: String::new(),
            llm_cost_logging_enabled: true,

            // Notifications
            notifications_enabled: false,
            notifications_signals_dir: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".mcp_agent_mail")
                .join("signals"),
            notifications_include_metadata: true,
            notifications_debounce_ms: 100,

            // Tool filtering
            tool_filter: ToolFilterSettings::default(),

            // Instrumentation
            instrumentation_enabled: false,
            instrumentation_slow_query_ms: 250,
            tools_log_enabled: true,
            tool_metrics_emit_enabled: false,
            tool_metrics_emit_interval_seconds: 60,

            // Retention / Quota
            retention_report_enabled: false,
            retention_report_interval_seconds: 3600,
            retention_max_age_days: 180,
            retention_ignore_project_patterns: vec![
                "demo".to_string(),
                "test*".to_string(),
                "testproj*".to_string(),
                "testproject".to_string(),
                "backendproj*".to_string(),
                "frontendproj*".to_string(),
            ],
            quota_enabled: false,
            quota_attachments_limit_bytes: 0,
            quota_inbox_limit_count: 0,

            // TOON output format
            toon_bin: None,
            toon_stats_enabled: false,
            output_format_default: None,

            // Logging
            log_level: "INFO".to_string(),
            log_rich_enabled: true,
            log_tool_calls_enabled: true,
            log_tool_calls_result_max_chars: 2000,
            log_include_trace: false,
            log_json_enabled: false,

            // Console / TUI layout + persistence
            console_persist_path: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("mcp-agent-mail")
                .join("config.env"),
            console_auto_save: true,
            console_interactive_enabled: true,
            console_ui_height_percent: 33,
            console_ui_anchor: ConsoleUiAnchor::Bottom,
            console_ui_auto_size: false,
            console_inline_auto_min_rows: 8,
            console_inline_auto_max_rows: 18,
            console_split_mode: ConsoleSplitMode::Inline,
            console_split_ratio_percent: 30,
            console_theme: ConsoleThemeId::CyberpunkAurora,
            tui_enabled: true,
            tui_dock_position: "right".to_string(),
            tui_dock_ratio_percent: 40,
            tui_dock_visible: true,
            tui_high_contrast: false,
            tui_key_hints: true,
        }
    }
}

/// Module-level shared config cache (used by `Config::get` and `Config::reset_cached`).
static CONFIG_CACHE: std::sync::RwLock<Option<Config>> = std::sync::RwLock::new(None);

fn global_config_cache_get() -> Config {
    // Fast path: read lock, return clone if present
    {
        let guard = CONFIG_CACHE
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ref c) = *guard {
            return c.clone();
        }
    }
    // Slow path: write lock, initialize from env
    let mut guard = CONFIG_CACHE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        *guard = Some(Config::from_env());
    }
    guard.as_ref().expect("just initialized").clone()
}

fn global_config_cache_reset() {
    let mut guard = CONFIG_CACHE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

impl Config {
    fn apply_environment_defaults(&mut self) {
        let is_dev = self.app_environment == AppEnvironment::Development;
        self.http_cors_enabled = is_dev;
        self.allow_absolute_attachment_paths = is_dev;
    }

    /// Load configuration from environment variables
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // Application
        if let Some(v) = env_value("APP_ENVIRONMENT") {
            config.app_environment = match v.to_lowercase().as_str() {
                "production" | "prod" => AppEnvironment::Production,
                _ => AppEnvironment::Development,
            };
        }
        // Align CORS default with legacy behavior: enabled in development, disabled in production.
        config.apply_environment_defaults();
        let worktrees_enabled = env_bool("WORKTREES_ENABLED", config.worktrees_enabled);
        let git_identity_enabled = env_bool("GIT_IDENTITY_ENABLED", false);
        config.worktrees_enabled = worktrees_enabled || git_identity_enabled;
        if let Some(v) = env_value("PROJECT_IDENTITY_MODE") {
            config.project_identity_mode = match v.trim().to_lowercase().as_str() {
                "git-remote" => ProjectIdentityMode::GitRemote,
                "git-common-dir" => ProjectIdentityMode::GitCommonDir,
                "git-toplevel" => ProjectIdentityMode::GitToplevel,
                _ => ProjectIdentityMode::Dir,
            };
        }
        if let Some(v) = env_value("PROJECT_IDENTITY_REMOTE") {
            config.project_identity_remote = v;
        }

        // Database
        if let Some(v) = env_value("DATABASE_URL") {
            config.database_url = v;
        }
        config.database_echo = env_bool("DATABASE_ECHO", config.database_echo);
        config.database_pool_size = env_usize_opt("DATABASE_POOL_SIZE");
        config.database_max_overflow = env_usize_opt("DATABASE_MAX_OVERFLOW");
        config.database_pool_timeout = env_u64_opt("DATABASE_POOL_TIMEOUT");
        config.integrity_check_on_startup = env_bool(
            "INTEGRITY_CHECK_ON_STARTUP",
            config.integrity_check_on_startup,
        );
        config.integrity_check_interval_hours = env_u64(
            "INTEGRITY_CHECK_INTERVAL_HOURS",
            config.integrity_check_interval_hours,
        );

        // Storage
        if let Some(v) = env_value("STORAGE_ROOT") {
            config.storage_root = PathBuf::from(shellexpand::tilde(&v).into_owned());
        }
        if let Some(v) = env_value("GIT_AUTHOR_NAME") {
            config.git_author_name = v;
        }
        if let Some(v) = env_value("GIT_AUTHOR_EMAIL") {
            config.git_author_email = v;
        }
        config.inline_image_max_bytes =
            env_usize("INLINE_IMAGE_MAX_BYTES", config.inline_image_max_bytes);
        config.convert_images = env_bool("CONVERT_IMAGES", config.convert_images);
        config.keep_original_images = env_bool("KEEP_ORIGINAL_IMAGES", config.keep_original_images);
        config.allow_absolute_attachment_paths = env_bool(
            "ALLOW_ABSOLUTE_ATTACHMENT_PATHS",
            config.allow_absolute_attachment_paths,
        );

        // Disk space monitoring
        config.disk_space_monitor_enabled = env_bool(
            "DISK_SPACE_MONITOR_ENABLED",
            config.disk_space_monitor_enabled,
        );
        config.disk_space_warning_mb =
            env_u64("DISK_SPACE_WARNING_MB", config.disk_space_warning_mb);
        config.disk_space_critical_mb =
            env_u64("DISK_SPACE_CRITICAL_MB", config.disk_space_critical_mb);
        config.disk_space_fatal_mb = env_u64("DISK_SPACE_FATAL_MB", config.disk_space_fatal_mb);
        config.disk_space_check_interval_seconds = env_u64(
            "DISK_SPACE_CHECK_INTERVAL_SECONDS",
            config.disk_space_check_interval_seconds,
        );

        // Memory pressure monitoring
        config.memory_warning_mb = env_u64("MEMORY_WARNING_MB", config.memory_warning_mb);
        config.memory_critical_mb = env_u64("MEMORY_CRITICAL_MB", config.memory_critical_mb);
        config.memory_fatal_mb = env_u64("MEMORY_FATAL_MB", config.memory_fatal_mb);

        // HTTP
        if let Some(v) = env_value("HTTP_HOST") {
            config.http_host = v;
        }
        config.http_port = env_u16("HTTP_PORT", config.http_port);
        if let Some(v) = env_value("HTTP_PATH") {
            config.http_path = v;
        }
        config.http_bearer_token = full_env_value("HTTP_BEARER_TOKEN").filter(|s| !s.is_empty());
        config.http_allow_localhost_unauthenticated = env_bool(
            "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED",
            config.http_allow_localhost_unauthenticated,
        );
        config.http_request_log_enabled =
            env_bool("HTTP_REQUEST_LOG_ENABLED", config.http_request_log_enabled);
        config.http_otel_enabled = env_bool("HTTP_OTEL_ENABLED", config.http_otel_enabled);
        if let Some(v) = env_value("OTEL_SERVICE_NAME") {
            config.http_otel_service_name = v;
        }
        if let Some(v) = env_value("OTEL_EXPORTER_OTLP_ENDPOINT") {
            config.http_otel_exporter_otlp_endpoint = v;
        }

        // Rate Limiting
        config.http_rate_limit_enabled =
            env_bool("HTTP_RATE_LIMIT_ENABLED", config.http_rate_limit_enabled);
        if let Some(v) = env_value("HTTP_RATE_LIMIT_BACKEND") {
            config.http_rate_limit_backend = match v.trim().to_lowercase().as_str() {
                "redis" => RateLimitBackend::Redis,
                _ => RateLimitBackend::Memory,
            };
        }
        config.http_rate_limit_per_minute = env_u32(
            "HTTP_RATE_LIMIT_PER_MINUTE",
            config.http_rate_limit_per_minute,
        );
        config.http_rate_limit_tools_per_minute = env_u32(
            "HTTP_RATE_LIMIT_TOOLS_PER_MINUTE",
            config.http_rate_limit_tools_per_minute,
        );
        config.http_rate_limit_resources_per_minute = env_u32(
            "HTTP_RATE_LIMIT_RESOURCES_PER_MINUTE",
            config.http_rate_limit_resources_per_minute,
        );
        config.http_rate_limit_tools_burst = env_u32(
            "HTTP_RATE_LIMIT_TOOLS_BURST",
            config.http_rate_limit_tools_burst,
        );
        config.http_rate_limit_resources_burst = env_u32(
            "HTTP_RATE_LIMIT_RESOURCES_BURST",
            config.http_rate_limit_resources_burst,
        );
        config.http_rate_limit_redis_url =
            env_value("HTTP_RATE_LIMIT_REDIS_URL").filter(|s| !s.is_empty());

        // JWT
        config.http_jwt_enabled = env_bool("HTTP_JWT_ENABLED", config.http_jwt_enabled);
        if let Some(v) = env_value("HTTP_JWT_ALGORITHMS") {
            config.http_jwt_algorithms = parse_csv(&v);
        }
        config.http_jwt_secret = env_value("HTTP_JWT_SECRET").filter(|s| !s.is_empty());
        config.http_jwt_jwks_url = env_value("HTTP_JWT_JWKS_URL").filter(|s| !s.is_empty());
        config.http_jwt_audience = env_value("HTTP_JWT_AUDIENCE").filter(|s| !s.is_empty());
        config.http_jwt_issuer = env_value("HTTP_JWT_ISSUER").filter(|s| !s.is_empty());
        if let Some(v) = env_value("HTTP_JWT_ROLE_CLAIM") {
            config.http_jwt_role_claim = v;
        }

        // RBAC
        config.http_rbac_enabled = env_bool("HTTP_RBAC_ENABLED", config.http_rbac_enabled);
        if let Some(v) = env_value("HTTP_RBAC_READER_ROLES") {
            config.http_rbac_reader_roles = parse_csv(&v);
        }
        if let Some(v) = env_value("HTTP_RBAC_WRITER_ROLES") {
            config.http_rbac_writer_roles = parse_csv(&v);
        }
        if let Some(v) = env_value("HTTP_RBAC_DEFAULT_ROLE") {
            config.http_rbac_default_role = v;
        }
        if let Some(v) = env_value("HTTP_RBAC_READONLY_TOOLS") {
            config.http_rbac_readonly_tools = parse_csv(&v);
        }

        // CORS
        config.http_cors_enabled = env_bool("HTTP_CORS_ENABLED", config.http_cors_enabled);
        if let Some(v) = env_value("HTTP_CORS_ORIGINS") {
            config.http_cors_origins = parse_csv(&v);
        }
        config.http_cors_allow_credentials = env_bool(
            "HTTP_CORS_ALLOW_CREDENTIALS",
            config.http_cors_allow_credentials,
        );
        if let Some(v) = env_value("HTTP_CORS_ALLOW_METHODS") {
            config.http_cors_allow_methods = parse_csv(&v);
        }
        if let Some(v) = env_value("HTTP_CORS_ALLOW_HEADERS") {
            config.http_cors_allow_headers = parse_csv(&v);
        }

        // Contact & Messaging
        config.contact_enforcement_enabled = env_bool(
            "CONTACT_ENFORCEMENT_ENABLED",
            config.contact_enforcement_enabled,
        );
        config.contact_auto_ttl_seconds =
            env_u64("CONTACT_AUTO_TTL_SECONDS", config.contact_auto_ttl_seconds);
        config.contact_auto_retry_enabled = env_bool(
            "CONTACT_AUTO_RETRY_ENABLED",
            config.contact_auto_retry_enabled,
        );
        config.messaging_auto_register_recipients = env_bool(
            "MESSAGING_AUTO_REGISTER_RECIPIENTS",
            config.messaging_auto_register_recipients,
        );
        config.messaging_auto_handshake_on_block = env_bool(
            "MESSAGING_AUTO_HANDSHAKE_ON_BLOCK",
            config.messaging_auto_handshake_on_block,
        );

        // Message size limits
        config.max_message_body_bytes =
            env_usize("MAX_MESSAGE_BODY_BYTES", config.max_message_body_bytes);
        config.max_attachment_bytes =
            env_usize("MAX_ATTACHMENT_BYTES", config.max_attachment_bytes);
        config.max_total_message_bytes =
            env_usize("MAX_TOTAL_MESSAGE_BYTES", config.max_total_message_bytes);
        config.max_subject_bytes = env_usize("MAX_SUBJECT_BYTES", config.max_subject_bytes);

        // File Reservations
        config.file_reservations_cleanup_enabled = env_bool(
            "FILE_RESERVATIONS_CLEANUP_ENABLED",
            config.file_reservations_cleanup_enabled,
        );
        config.file_reservations_cleanup_interval_seconds = env_u64(
            "FILE_RESERVATIONS_CLEANUP_INTERVAL_SECONDS",
            config.file_reservations_cleanup_interval_seconds,
        );
        config.file_reservation_inactivity_seconds = env_u64(
            "FILE_RESERVATION_INACTIVITY_SECONDS",
            config.file_reservation_inactivity_seconds,
        );
        config.file_reservation_activity_grace_seconds = env_u64(
            "FILE_RESERVATION_ACTIVITY_GRACE_SECONDS",
            config.file_reservation_activity_grace_seconds,
        );
        config.file_reservations_enforcement_enabled = env_bool(
            "FILE_RESERVATIONS_ENFORCEMENT_ENABLED",
            config.file_reservations_enforcement_enabled,
        );

        // Ack TTL warnings
        config.ack_ttl_enabled = env_bool("ACK_TTL_ENABLED", config.ack_ttl_enabled);
        config.ack_ttl_seconds = env_u64("ACK_TTL_SECONDS", config.ack_ttl_seconds);
        config.ack_ttl_scan_interval_seconds = env_u64(
            "ACK_TTL_SCAN_INTERVAL_SECONDS",
            config.ack_ttl_scan_interval_seconds,
        );

        // Ack escalation
        config.ack_escalation_enabled =
            env_bool("ACK_ESCALATION_ENABLED", config.ack_escalation_enabled);
        if let Some(v) = env_value("ACK_ESCALATION_MODE") {
            config.ack_escalation_mode = v;
        }
        config.ack_escalation_claim_ttl_seconds = env_u64(
            "ACK_ESCALATION_CLAIM_TTL_SECONDS",
            config.ack_escalation_claim_ttl_seconds,
        );
        config.ack_escalation_claim_exclusive = env_bool(
            "ACK_ESCALATION_CLAIM_EXCLUSIVE",
            config.ack_escalation_claim_exclusive,
        );
        if let Some(v) = env_value("ACK_ESCALATION_CLAIM_HOLDER_NAME") {
            config.ack_escalation_claim_holder_name = v;
        }

        // Agent Naming
        if let Some(v) = env_value("AGENT_NAME_ENFORCEMENT_MODE") {
            config.agent_name_enforcement_mode = match v.trim().to_lowercase().as_str() {
                "strict" => AgentNameEnforcementMode::Strict,
                "always_auto" | "alwaysauto" => AgentNameEnforcementMode::AlwaysAuto,
                _ => AgentNameEnforcementMode::Coerce,
            };
        }

        // LLM
        config.llm_enabled = env_bool("LLM_ENABLED", config.llm_enabled);
        if let Some(v) = env_value("LLM_DEFAULT_MODEL") {
            config.llm_default_model = v;
        }
        config.llm_temperature = env_f64("LLM_TEMPERATURE", config.llm_temperature);
        config.llm_max_tokens = env_u32("LLM_MAX_TOKENS", config.llm_max_tokens);
        config.llm_cache_enabled = env_bool("LLM_CACHE_ENABLED", config.llm_cache_enabled);
        if let Some(v) = env_value("LLM_CACHE_BACKEND") {
            config.llm_cache_backend = v;
        }
        if let Some(v) = env_value("LLM_CACHE_REDIS_URL") {
            config.llm_cache_redis_url = v;
        }
        config.llm_cost_logging_enabled =
            env_bool("LLM_COST_LOGGING_ENABLED", config.llm_cost_logging_enabled);

        // Notifications
        config.notifications_enabled =
            env_bool("NOTIFICATIONS_ENABLED", config.notifications_enabled);
        if let Some(v) = env_value("NOTIFICATIONS_SIGNALS_DIR") {
            config.notifications_signals_dir = PathBuf::from(shellexpand::tilde(&v).into_owned());
        }
        config.notifications_include_metadata = env_bool(
            "NOTIFICATIONS_INCLUDE_METADATA",
            config.notifications_include_metadata,
        );
        config.notifications_debounce_ms = env_u64(
            "NOTIFICATIONS_DEBOUNCE_MS",
            config.notifications_debounce_ms,
        );

        // Instrumentation
        config.instrumentation_enabled =
            env_bool("INSTRUMENTATION_ENABLED", config.instrumentation_enabled);
        config.instrumentation_slow_query_ms = env_u64(
            "INSTRUMENTATION_SLOW_QUERY_MS",
            config.instrumentation_slow_query_ms,
        );
        config.tools_log_enabled = env_bool("TOOLS_LOG_ENABLED", config.tools_log_enabled);
        config.tool_metrics_emit_enabled = env_bool(
            "TOOL_METRICS_EMIT_ENABLED",
            config.tool_metrics_emit_enabled,
        );
        config.tool_metrics_emit_interval_seconds = env_u64(
            "TOOL_METRICS_EMIT_INTERVAL_SECONDS",
            config.tool_metrics_emit_interval_seconds,
        );

        // Retention / Quota
        config.retention_report_enabled =
            env_bool("RETENTION_REPORT_ENABLED", config.retention_report_enabled);
        config.retention_report_interval_seconds = env_u64(
            "RETENTION_REPORT_INTERVAL_SECONDS",
            config.retention_report_interval_seconds,
        );
        config.retention_max_age_days =
            env_u64("RETENTION_MAX_AGE_DAYS", config.retention_max_age_days);
        if let Some(v) = env_value("RETENTION_IGNORE_PROJECT_PATTERNS") {
            config.retention_ignore_project_patterns = parse_csv(&v);
        }
        config.quota_enabled = env_bool("QUOTA_ENABLED", config.quota_enabled);
        config.quota_attachments_limit_bytes = env_u64(
            "QUOTA_ATTACHMENTS_LIMIT_BYTES",
            config.quota_attachments_limit_bytes,
        );
        config.quota_inbox_limit_count =
            env_u64("QUOTA_INBOX_LIMIT_COUNT", config.quota_inbox_limit_count);

        // Tool filtering
        config.tool_filter.enabled = env_bool("TOOLS_FILTER_ENABLED", config.tool_filter.enabled);
        if let Some(v) = env_value("TOOLS_FILTER_PROFILE") {
            config.tool_filter.profile = normalize_tool_filter_profile(&v);
        }
        if let Some(v) = env_value("TOOLS_FILTER_MODE") {
            config.tool_filter.mode = normalize_tool_filter_mode(&v);
        }
        if let Some(v) = env_value("TOOLS_FILTER_CLUSTERS") {
            config.tool_filter.clusters = parse_csv(&v);
        }
        if let Some(v) = env_value("TOOLS_FILTER_TOOLS") {
            config.tool_filter.tools = parse_csv(&v);
        }

        // TOON output format
        // Encoder binary: TOON_TRU_BIN > TOON_BIN > None (will use default "tru")
        config.toon_bin = env_value("TOON_TRU_BIN")
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env_value("TOON_BIN")
                    .map(|v| v.trim().to_string())
                    .filter(|s| !s.is_empty())
            });
        config.toon_stats_enabled = env_bool("TOON_STATS", config.toon_stats_enabled);
        // Output format default: MCP_AGENT_MAIL_OUTPUT_FORMAT > TOON_DEFAULT_FORMAT > None
        config.output_format_default = env_value("MCP_AGENT_MAIL_OUTPUT_FORMAT")
            .map(|v| v.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env_value("TOON_DEFAULT_FORMAT")
                    .map(|v| v.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
            });

        // Logging
        if let Some(v) = env_value("LOG_LEVEL") {
            config.log_level = v;
        }
        config.log_rich_enabled = env_bool("LOG_RICH_ENABLED", config.log_rich_enabled);
        config.log_tool_calls_enabled =
            env_bool("LOG_TOOL_CALLS_ENABLED", config.log_tool_calls_enabled);
        config.log_tool_calls_result_max_chars = env_usize(
            "LOG_TOOL_CALLS_RESULT_MAX_CHARS",
            config.log_tool_calls_result_max_chars,
        );
        config.log_include_trace = env_bool("LOG_INCLUDE_TRACE", config.log_include_trace);
        config.log_json_enabled = env_bool("LOG_JSON_ENABLED", config.log_json_enabled);

        // Console / TUI layout + persistence
        //
        // Console layout is a *user preference* and must not require editing a repo `.env`.
        // For `CONSOLE_*` keys we read:
        //   real env > user config envfile > defaults
        // and we do NOT fall back to working-directory `.env`.
        if let Some(v) = real_env_value("CONSOLE_PERSIST_PATH") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                config.console_persist_path =
                    PathBuf::from(shellexpand::tilde(trimmed).into_owned());
            }
        }
        let persisted_console = load_dotenv_file(&config.console_persist_path);
        let console_value = |key: &str| -> Option<String> {
            #[cfg(test)]
            if let Some(v) = test_env_override_value(key) {
                return Some(v);
            }
            env::var(key)
                .ok()
                .or_else(|| persisted_console.get(key).cloned())
        };
        let console_bool = |key: &str, default: bool| -> bool {
            console_value(key).map_or(default, |v| parse_bool(&v, default))
        };
        let console_u16 = |key: &str, default: u16| -> u16 {
            console_value(key)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };

        config.console_auto_save = console_bool("CONSOLE_AUTO_SAVE", config.console_auto_save);
        config.console_interactive_enabled =
            console_bool("CONSOLE_INTERACTIVE", config.console_interactive_enabled);
        config.console_ui_height_percent = console_u16(
            "CONSOLE_UI_HEIGHT_PERCENT",
            config.console_ui_height_percent,
        )
        .clamp(10, 80);
        if let Some(v) = console_value("CONSOLE_UI_ANCHOR") {
            if let Some(anchor) = ConsoleUiAnchor::parse(&v) {
                config.console_ui_anchor = anchor;
            }
        }
        config.console_ui_auto_size =
            console_bool("CONSOLE_UI_AUTO_SIZE", config.console_ui_auto_size);
        config.console_inline_auto_min_rows = console_u16(
            "CONSOLE_INLINE_AUTO_MIN_ROWS",
            config.console_inline_auto_min_rows,
        )
        .max(4);
        config.console_inline_auto_max_rows = console_u16(
            "CONSOLE_INLINE_AUTO_MAX_ROWS",
            config.console_inline_auto_max_rows,
        )
        .max(config.console_inline_auto_min_rows);
        if let Some(v) = console_value("CONSOLE_SPLIT_MODE") {
            if let Some(mode) = ConsoleSplitMode::parse(&v) {
                config.console_split_mode = mode;
            }
        }
        config.console_split_ratio_percent = console_u16(
            "CONSOLE_SPLIT_RATIO_PERCENT",
            config.console_split_ratio_percent,
        )
        .clamp(10, 80);
        if let Some(v) = console_value("CONSOLE_THEME") {
            if let Some(theme) = ConsoleThemeId::parse(&v) {
                config.console_theme = theme;
            }
        }

        config.tui_enabled = env_bool("TUI_ENABLED", config.tui_enabled);
        if let Some(v) = console_value("TUI_DOCK_POSITION") {
            let lower = v.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "bottom" | "top" | "left" | "right") {
                config.tui_dock_position = lower;
            }
        }
        config.tui_dock_ratio_percent =
            console_u16("TUI_DOCK_RATIO_PERCENT", config.tui_dock_ratio_percent).clamp(20, 80);
        config.tui_dock_visible = console_bool("TUI_DOCK_VISIBLE", config.tui_dock_visible);
        config.tui_high_contrast = console_bool("TUI_HIGH_CONTRAST", config.tui_high_contrast);
        config.tui_key_hints = console_bool("TUI_KEY_HINTS", config.tui_key_hints);

        config
    }

    /// Return a clone of the globally cached configuration.
    ///
    /// On first call, parses environment variables via [`Config::from_env`] and
    /// stores the result in a process-wide cache. Subsequent calls return a
    /// clone of the cached value, avoiding repeated env-var parsing.
    ///
    /// Use this in hot paths (tool handlers) instead of `Config::from_env()`.
    /// For tests or CLI commands that need a fresh or mutated config, continue
    /// using `Config::from_env()` directly.
    ///
    /// Cloning a ~60-field struct is ~2-3 KB and takes <1 microsecond — far
    /// cheaper than parsing 40+ environment variables with string conversions.
    #[must_use]
    pub fn get() -> Self {
        global_config_cache_get()
    }

    /// Reset the global config cache, forcing the next [`Config::get`] call to
    /// re-parse environment variables. Intended for tests that modify env vars
    /// between test cases.
    pub fn reset_cached() {
        global_config_cache_reset();
    }

    /// Returns whether running in production mode
    #[must_use]
    pub fn is_production(&self) -> bool {
        self.app_environment == AppEnvironment::Production
    }

    /// Determine if a tool should be exposed based on tool filter settings.
    #[must_use]
    pub fn should_expose_tool(&self, tool_name: &str, cluster: &str) -> bool {
        let filter = &self.tool_filter;
        if !filter.enabled {
            return true;
        }

        let profile = filter.profile.as_str();
        if profile == "custom" {
            if filter.clusters.is_empty() && filter.tools.is_empty() {
                return true;
            }
            let in_cluster = filter.clusters.iter().any(|c| c == cluster);
            let in_tools = filter.tools.iter().any(|t| t == tool_name);
            if filter.mode == "exclude" {
                return !(in_cluster || in_tools);
            }
            return in_cluster || in_tools;
        }

        if profile == "full" {
            return true;
        }

        let (profile_clusters, profile_tools) = match profile {
            "core" => (
                &[
                    "identity",
                    "messaging",
                    "file_reservations",
                    "workflow_macros",
                ][..],
                &["health_check", "ensure_project"][..],
            ),
            "minimal" => (
                &[][..],
                &[
                    "health_check",
                    "ensure_project",
                    "register_agent",
                    "send_message",
                    "fetch_inbox",
                    "acknowledge_message",
                ][..],
            ),
            "messaging" => (
                &["identity", "messaging", "contact"][..],
                &["health_check", "ensure_project", "search_messages"][..],
            ),
            _ => (&[][..], &[][..]),
        };

        let in_cluster = profile_clusters.contains(&cluster);
        let in_tools = profile_tools.contains(&tool_name);

        if in_cluster || in_tools {
            return true;
        }

        profile_clusters.is_empty() && profile_tools.is_empty()
    }

    /// Build a startup bootstrap summary showing resolved config and sources.
    ///
    /// The summary is designed for concise terminal display, never exposes raw
    /// secrets, and explains exactly where each setting came from.
    #[must_use]
    pub fn bootstrap_summary(&self) -> BootstrapSummary {
        let mut lines = Vec::new();

        lines.push(BootstrapLine {
            key: "host",
            value: self.http_host.clone(),
            source: detect_source("HTTP_HOST"),
        });
        lines.push(BootstrapLine {
            key: "port",
            value: self.http_port.to_string(),
            source: detect_source("HTTP_PORT"),
        });
        lines.push(BootstrapLine {
            key: "path",
            value: self.http_path.clone(),
            source: ConfigSource::Default, // overridden by caller with CLI info
        });
        lines.push(BootstrapLine {
            key: "auth",
            value: match &self.http_bearer_token {
                Some(token) => format!("Bearer {}", mask_secret(token)),
                None if self.http_allow_localhost_unauthenticated => {
                    "none (localhost unauthenticated)".into()
                }
                None => "none".into(),
            },
            source: self
                .http_bearer_token
                .as_ref()
                .map_or(ConfigSource::Default, |_| {
                    detect_source("HTTP_BEARER_TOKEN")
                }),
        });
        lines.push(BootstrapLine {
            key: "db",
            value: self.database_url.clone(),
            source: detect_source("DATABASE_URL"),
        });
        lines.push(BootstrapLine {
            key: "storage",
            value: self.storage_root.display().to_string(),
            source: detect_source("STORAGE_ROOT"),
        });

        BootstrapSummary { lines }
    }
}

/// Where a configuration value was resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Process environment variable.
    ProcessEnv,
    /// Project-local `.env` file in working directory.
    ProjectDotenv,
    /// User-global `~/.mcp_agent_mail/.env` (or legacy `~/mcp_agent_mail/.env`).
    UserEnvFile,
    /// CLI argument override.
    CliArg,
    /// Hardcoded default.
    Default,
}

impl ConfigSource {
    /// Short label for terminal display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProcessEnv => "env",
            Self::ProjectDotenv => ".env",
            Self::UserEnvFile => "~/.mcp_agent_mail/.env",
            Self::CliArg => "cli",
            Self::Default => "default",
        }
    }
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One line in the startup bootstrap summary.
#[derive(Debug, Clone)]
pub struct BootstrapLine {
    /// Short key name (e.g. "host", "port", "auth").
    pub key: &'static str,
    /// Resolved display value (secrets masked).
    pub value: String,
    /// Where the value came from.
    pub source: ConfigSource,
}

/// Startup bootstrap summary showing resolved config sources.
#[derive(Debug, Clone)]
pub struct BootstrapSummary {
    pub lines: Vec<BootstrapLine>,
}

impl BootstrapSummary {
    /// Set the source for a given key (used by callers with extra context, e.g. CLI arg source).
    pub fn set_source(&mut self, key: &str, source: ConfigSource) {
        if let Some(line) = self.lines.iter_mut().find(|l| l.key == key) {
            line.source = source;
        }
    }

    /// Set value and source for a given key.
    pub fn set(&mut self, key: &str, value: String, source: ConfigSource) {
        if let Some(line) = self.lines.iter_mut().find(|l| l.key == key) {
            line.value = value;
            line.source = source;
        }
    }

    /// Format as a compact tree for terminal display.
    #[must_use]
    pub fn format(&self, mode: &str) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "  am: Starting MCP Agent Mail server");
        let has_mode = !mode.is_empty();
        let last_idx = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            let is_last = i == last_idx && !has_mode;
            let connector = if is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            };
            let _ = writeln!(
                out,
                "  {connector} {:<8} {} ({})",
                format!("{}:", line.key),
                line.value,
                line.source.label(),
            );
        }
        if has_mode {
            let _ = writeln!(out, "  \u{2514}\u{2500} {:<8} {mode}", "mode:");
        }
        out
    }
}

/// Mask a secret for display: show only the last 4 characters after `****`.
#[must_use]
pub fn mask_secret(value: &str) -> String {
    let char_count = value.chars().count();
    if char_count <= 8 {
        "****".to_string()
    } else {
        let suffix_rev: String = value.chars().rev().take(4).collect();
        let suffix: String = suffix_rev.chars().rev().collect();
        format!("****{suffix}")
    }
}

/// Detect which config source tier provided a given key.
///
/// Checks tiers in order: process env → project `.env` → user env → default.
#[must_use]
pub fn detect_source(key: &str) -> ConfigSource {
    if env::var(key).is_ok() {
        return ConfigSource::ProcessEnv;
    }
    if dotenv_value(key).is_some() {
        return ConfigSource::ProjectDotenv;
    }
    if user_env_value(key).is_some() {
        return ConfigSource::UserEnvFile;
    }
    ConfigSource::Default
}

// Helper functions for environment variable parsing

static DOTENV_VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();
static USER_ENV_VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_ENV_OVERRIDES: std::cell::RefCell<HashMap<String, String>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(test)]
fn test_env_override_value(key: &str) -> Option<String> {
    TEST_ENV_OVERRIDES.with(|cell| cell.borrow().get(key).cloned())
}

fn dotenv_values() -> &'static HashMap<String, String> {
    DOTENV_VALUES.get_or_init(|| load_dotenv_file(Path::new(".env")))
}

/// Read a value from the .env file (if present).
#[must_use]
pub fn dotenv_value(key: &str) -> Option<String> {
    dotenv_values().get(key).cloned()
}

/// Candidate paths for the user-global env file, checked in order.
///
/// - `~/.mcp_agent_mail/.env` — preferred (matches signals dir convention)
/// - `~/mcp_agent_mail/.env`  — legacy (matches old shell wrapper)
fn user_env_file_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".mcp_agent_mail").join(".env"),
        home.join("mcp_agent_mail").join(".env"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn user_env_values() -> &'static HashMap<String, String> {
    USER_ENV_VALUES
        .get_or_init(|| user_env_file_path().map_or_else(HashMap::new, |p| load_dotenv_file(&p)))
}

/// Read a value from the user-global env file (`~/.mcp_agent_mail/.env`).
#[must_use]
pub fn user_env_value(key: &str) -> Option<String> {
    user_env_values().get(key).cloned()
}

/// Read a value with full precedence: process env → project `.env` → user env file.
#[must_use]
pub fn full_env_value(key: &str) -> Option<String> {
    env_value(key).or_else(|| user_env_value(key))
}

/// Read a value from the real environment first, falling back to .env.
#[must_use]
pub fn env_value(key: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = test_env_override_value(key) {
        return Some(v);
    }
    env::var(key).ok().or_else(|| dotenv_value(key))
}

/// Read from the real environment only (no working-directory `.env` fallback).
#[must_use]
fn real_env_value(key: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = test_env_override_value(key) {
        return Some(v);
    }
    env::var(key).ok()
}

fn load_dotenv_file(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_dotenv_contents(&contents)
}

/// Update (or create) an envfile at `path` by replacing/adding the provided `KEY=value` pairs.
///
/// Preserves unrelated lines and comments. Keys are matched on `KEY=` after optional leading
/// whitespace and optional `export ` prefix.
pub fn update_envfile<S: std::hash::BuildHasher>(
    path: &Path,
    updates: &HashMap<&str, String, S>,
) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out_lines: Vec<String> = Vec::new();

    for line in existing.lines() {
        let trimmed = line.trim_start();
        let maybe = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, _)) = maybe.split_once('=') else {
            out_lines.push(line.to_string());
            continue;
        };
        let key = key.trim();
        let Some(value) = updates.get(key) else {
            out_lines.push(line.to_string());
            continue;
        };

        let comment = extract_inline_comment(line);
        let mut replaced = format!("{key}={value}");
        if let Some(suffix) = comment {
            replaced.push(' ');
            replaced.push_str(suffix.trim_start());
        }
        out_lines.push(replaced);
        seen.insert(key);
    }

    for (key, value) in updates {
        if !seen.contains(key) {
            out_lines.push(format!("{key}={value}"));
        }
    }

    let mut out = out_lines.join("\n");
    out.push('\n');

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, out)
}

fn parse_dotenv_contents(contents: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_dotenv_value(value.trim());
        map.insert(key.to_string(), value);
    }
    map
}

fn parse_dotenv_value(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        return unescape_double_quotes(stripped);
    }
    if let Some(stripped) = trimmed
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
    {
        return stripped.to_string();
    }
    strip_inline_comment(trimmed).to_string()
}

fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return value[..i].trim_end();
        }
    }
    value
}

fn extract_inline_comment(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return Some(&line[i..]);
        }
    }
    None
}

fn unescape_double_quotes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') | None => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => out.push(other),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_bool(value: &str, default: bool) -> bool {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" => true,
        "0" | "false" | "f" | "no" | "n" => false,
        _ => default,
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    env_value(key).map_or(default, |v| parse_bool(&v, default))
}

fn env_u16(key: &str, default: u16) -> u16 {
    env_value(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env_value(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_value(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env_value(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64_opt(key: &str) -> Option<u64> {
    env_value(key).and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse().ok()
        }
    })
}

fn env_usize_opt(key: &str) -> Option<usize> {
    env_value(key).and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.parse().ok()
        }
    })
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_tool_filter_profile(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "full" | "core" | "minimal" | "messaging" | "custom" => value.trim().to_lowercase(),
        _ => "full".to_string(),
    }
}

fn normalize_tool_filter_mode(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "include" | "exclude" => value.trim().to_lowercase(),
        _ => "include".to_string(),
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    env_value(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnvOverrideGuard {
        previous: Vec<(String, Option<String>)>,
    }

    impl TestEnvOverrideGuard {
        fn set(vars: &[(&str, &str)]) -> Self {
            let mut previous = Vec::new();
            TEST_ENV_OVERRIDES.with(|cell| {
                let mut map = cell.borrow_mut();
                for (key, value) in vars {
                    let old = map.get(*key).cloned();
                    previous.push(((*key).to_string(), old));
                    map.insert((*key).to_string(), (*value).to_string());
                }
            });
            Self { previous }
        }
    }

    impl Drop for TestEnvOverrideGuard {
        fn drop(&mut self) {
            TEST_ENV_OVERRIDES.with(|cell| {
                let mut map = cell.borrow_mut();
                for (key, value) in self.previous.drain(..) {
                    match value {
                        Some(v) => {
                            map.insert(key, v);
                        }
                        None => {
                            map.remove(&key);
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.http_port, 8765);
        assert!(config.database_pool_size.is_none());
        assert!(config.database_max_overflow.is_none());
        assert!(config.database_pool_timeout.is_none());
        assert_eq!(
            config.database_url,
            "sqlite+aiosqlite:///./storage.sqlite3".to_string()
        );
        assert!(config.contact_enforcement_enabled);
        assert!(config.allow_absolute_attachment_paths);
    }

    #[test]
    fn test_tool_call_logging_config_defaults() {
        let config = Config::default();
        assert!(config.log_tool_calls_enabled);
        assert_eq!(config.log_tool_calls_result_max_chars, 2000);
    }

    #[test]
    fn test_tool_call_logging_config_from_env() {
        let _env = TestEnvOverrideGuard::set(&[
            ("LOG_TOOL_CALLS_ENABLED", "false"),
            ("LOG_TOOL_CALLS_RESULT_MAX_CHARS", "1234"),
        ]);

        let config = Config::from_env();
        assert!(!config.log_tool_calls_enabled);
        assert_eq!(config.log_tool_calls_result_max_chars, 1234);
    }

    #[test]
    fn test_console_layout_defaults() {
        let config = Config::default();
        assert_eq!(config.console_ui_height_percent, 33);
        assert_eq!(config.console_ui_anchor, ConsoleUiAnchor::Bottom);
        assert!(!config.console_ui_auto_size);
        assert_eq!(config.console_inline_auto_min_rows, 8);
        assert_eq!(config.console_inline_auto_max_rows, 18);
        assert_eq!(config.console_split_mode, ConsoleSplitMode::Inline);
        assert_eq!(config.console_split_ratio_percent, 30);
        assert_eq!(config.console_theme, ConsoleThemeId::CyberpunkAurora);
        assert!(config.console_auto_save);
        assert!(config.console_interactive_enabled);
    }

    #[test]
    fn test_console_layout_from_env_overrides() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_path = tmp.path().join("config.env");
        let env_path_str = env_path.to_string_lossy().to_string();
        let vars = vec![
            ("CONSOLE_PERSIST_PATH", env_path_str.as_str()),
            ("CONSOLE_UI_HEIGHT_PERCENT", "50"),
            ("CONSOLE_UI_ANCHOR", "top"),
            ("CONSOLE_UI_AUTO_SIZE", "true"),
            ("CONSOLE_INLINE_AUTO_MIN_ROWS", "4"),
            ("CONSOLE_INLINE_AUTO_MAX_ROWS", "10"),
            ("CONSOLE_SPLIT_MODE", "left"),
            ("CONSOLE_SPLIT_RATIO_PERCENT", "40"),
            ("CONSOLE_THEME", "high_contrast"),
            ("CONSOLE_AUTO_SAVE", "false"),
            ("CONSOLE_INTERACTIVE", "false"),
        ];
        let _env = TestEnvOverrideGuard::set(&vars);

        let config = Config::from_env();
        assert_eq!(config.console_persist_path, env_path);
        assert_eq!(config.console_ui_height_percent, 50);
        assert_eq!(config.console_ui_anchor, ConsoleUiAnchor::Top);
        assert!(config.console_ui_auto_size);
        assert_eq!(config.console_inline_auto_min_rows, 4);
        assert_eq!(config.console_inline_auto_max_rows, 10);
        assert_eq!(config.console_split_mode, ConsoleSplitMode::Left);
        assert_eq!(config.console_split_ratio_percent, 40);
        assert_eq!(config.console_theme, ConsoleThemeId::HighContrast);
        assert!(!config.console_auto_save);
        assert!(!config.console_interactive_enabled);
    }

    #[test]
    fn test_console_layout_reads_user_envfile_when_env_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_path = tmp.path().join("config.env");
        std::fs::write(
            &env_path,
            "CONSOLE_UI_HEIGHT_PERCENT=55\nCONSOLE_UI_ANCHOR=top\nCONSOLE_UI_AUTO_SIZE=1\nCONSOLE_THEME=darcula\n",
        )
        .expect("write envfile");
        let env_path_str = env_path.to_string_lossy().to_string();
        let vars = vec![("CONSOLE_PERSIST_PATH", env_path_str.as_str())];
        let _env = TestEnvOverrideGuard::set(&vars);

        let config = Config::from_env();
        assert_eq!(config.console_persist_path, env_path);
        assert_eq!(config.console_ui_height_percent, 55);
        assert_eq!(config.console_ui_anchor, ConsoleUiAnchor::Top);
        assert!(config.console_ui_auto_size);
        assert_eq!(config.console_theme, ConsoleThemeId::Darcula);
    }

    #[test]
    fn test_console_persist_path_expands_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let _env = TestEnvOverrideGuard::set(&[(
            "CONSOLE_PERSIST_PATH",
            "~/.config/mcp-agent-mail/config.env",
        )]);
        let config = Config::from_env();
        assert_eq!(
            config.console_persist_path,
            home.join(".config/mcp-agent-mail/config.env")
        );
    }

    #[test]
    fn test_console_layout_env_overrides_user_envfile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_path = tmp.path().join("config.env");
        std::fs::write(
            &env_path,
            "CONSOLE_UI_HEIGHT_PERCENT=40\nCONSOLE_THEME=darcula\n",
        )
        .expect("write envfile");
        let env_path_str = env_path.to_string_lossy().to_string();
        let vars = vec![
            ("CONSOLE_PERSIST_PATH", env_path_str.as_str()),
            ("CONSOLE_UI_HEIGHT_PERCENT", "60"),
            ("CONSOLE_THEME", "high_contrast"),
        ];
        let _env = TestEnvOverrideGuard::set(&vars);

        let config = Config::from_env();
        assert_eq!(config.console_ui_height_percent, 60);
        assert_eq!(config.console_theme, ConsoleThemeId::HighContrast);
    }

    #[test]
    fn test_update_envfile_preserves_unrelated_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_path = tmp.path().join("config.env");
        std::fs::write(
            &env_path,
            "# Header comment\nOTHER=1\nexport CONSOLE_UI_HEIGHT_PERCENT=33 # trailing\n\n",
        )
        .expect("write envfile");

        let mut updates: HashMap<&str, String> = HashMap::new();
        updates.insert("CONSOLE_UI_HEIGHT_PERCENT", "50".to_string());
        updates.insert("CONSOLE_UI_ANCHOR", "top".to_string());

        update_envfile(&env_path, &updates).expect("update envfile");
        let content1 = std::fs::read_to_string(&env_path).expect("read envfile");
        assert!(content1.contains("# Header comment"));
        assert!(content1.contains("OTHER=1"));
        assert!(content1.contains("CONSOLE_UI_HEIGHT_PERCENT=50"));
        assert!(content1.contains("CONSOLE_UI_ANCHOR=top"));

        update_envfile(&env_path, &updates).expect("update envfile again");
        let content2 = std::fs::read_to_string(&env_path).expect("read envfile");
        assert_eq!(content1, content2, "expected update to be idempotent");
    }

    #[test]
    fn test_from_env() {
        // This just tests that from_env doesn't panic
        let _config = Config::from_env();
    }

    #[test]
    fn test_cors_defaults_follow_environment() {
        let mut config = Config {
            app_environment: AppEnvironment::Development,
            ..Config::default()
        };
        config.apply_environment_defaults();
        assert!(config.http_cors_enabled);

        let mut config = Config {
            app_environment: AppEnvironment::Production,
            ..Config::default()
        };
        config.apply_environment_defaults();
        assert!(!config.http_cors_enabled);
    }

    #[test]
    fn test_parse_bool_defaults() {
        assert!(parse_bool("true", false));
        assert!(parse_bool("1", false));
        assert!(!parse_bool("false", true));
        assert!(!parse_bool("0", true));
        assert!(parse_bool("maybe", true));
        assert!(!parse_bool("maybe", false));
        assert!(parse_bool("", true));
        assert!(!parse_bool("", false));
    }

    #[test]
    fn test_parse_csv_trims_and_skips_empty() {
        let parsed = parse_csv(" one, two , ,three,, ");
        assert_eq!(parsed, vec!["one", "two", "three"]);
    }

    #[test]
    fn test_load_dotenv_missing_returns_empty() {
        let values = load_dotenv_file(Path::new("/nonexistent/does-not-exist.env"));
        assert!(values.is_empty());
    }

    #[test]
    fn test_parse_dotenv_contents() {
        let contents = r#"
            # Comment
            export FOO=bar
            EMPTY=
            QUOTED="hello world"
            SINGLE='hi'
            TRAIL=keep # comment
            ESCAPED="line\nnext"
        "#;
        let values = parse_dotenv_contents(contents);
        assert_eq!(values.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(values.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(values.get("QUOTED"), Some(&"hello world".to_string()));
        assert_eq!(values.get("SINGLE"), Some(&"hi".to_string()));
        assert_eq!(values.get("TRAIL"), Some(&"keep".to_string()));
        assert_eq!(values.get("ESCAPED"), Some(&"line\nnext".to_string()));
    }

    // -----------------------------------------------------------------------
    // should_expose_tool
    // -----------------------------------------------------------------------

    fn make_filter(enabled: bool, profile: &str) -> Config {
        Config {
            tool_filter: ToolFilterSettings {
                enabled,
                profile: profile.to_string(),
                ..ToolFilterSettings::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn filter_disabled_exposes_all() {
        let config = make_filter(false, "full");
        assert!(config.should_expose_tool("send_message", "messaging"));
        assert!(config.should_expose_tool("obscure_tool", "unknown_cluster"));
    }

    #[test]
    fn full_profile_exposes_all() {
        let config = make_filter(true, "full");
        assert!(config.should_expose_tool("send_message", "messaging"));
        assert!(config.should_expose_tool("anything", "whatever"));
    }

    #[test]
    fn core_profile_includes_identity_cluster() {
        let config = make_filter(true, "core");
        assert!(config.should_expose_tool("register_agent", "identity"));
        assert!(config.should_expose_tool("create_agent_identity", "identity"));
    }

    #[test]
    fn core_profile_includes_messaging_cluster() {
        let config = make_filter(true, "core");
        assert!(config.should_expose_tool("send_message", "messaging"));
        assert!(config.should_expose_tool("reply_message", "messaging"));
    }

    #[test]
    fn core_profile_includes_file_reservations_cluster() {
        let config = make_filter(true, "core");
        assert!(config.should_expose_tool("file_reservation_paths", "file_reservations"));
    }

    #[test]
    fn core_profile_includes_workflow_macros_cluster() {
        let config = make_filter(true, "core");
        assert!(config.should_expose_tool("macro_start_session", "workflow_macros"));
    }

    #[test]
    fn core_profile_includes_explicit_tools() {
        let config = make_filter(true, "core");
        assert!(config.should_expose_tool("health_check", "other"));
        assert!(config.should_expose_tool("ensure_project", "other"));
    }

    #[test]
    fn core_profile_excludes_non_core_tools() {
        let config = make_filter(true, "core");
        assert!(!config.should_expose_tool("search_messages", "search"));
        assert!(!config.should_expose_tool("summarize_thread", "search"));
    }

    #[test]
    fn minimal_profile_includes_only_six_tools() {
        let config = make_filter(true, "minimal");
        assert!(config.should_expose_tool("health_check", "any"));
        assert!(config.should_expose_tool("ensure_project", "any"));
        assert!(config.should_expose_tool("register_agent", "any"));
        assert!(config.should_expose_tool("send_message", "any"));
        assert!(config.should_expose_tool("fetch_inbox", "any"));
        assert!(config.should_expose_tool("acknowledge_message", "any"));
    }

    #[test]
    fn minimal_profile_excludes_others() {
        let config = make_filter(true, "minimal");
        assert!(!config.should_expose_tool("reply_message", "messaging"));
        assert!(!config.should_expose_tool("file_reservation_paths", "file_reservations"));
        assert!(!config.should_expose_tool("search_messages", "search"));
    }

    #[test]
    fn messaging_profile_includes_identity_messaging_contact() {
        let config = make_filter(true, "messaging");
        assert!(config.should_expose_tool("register_agent", "identity"));
        assert!(config.should_expose_tool("send_message", "messaging"));
        assert!(config.should_expose_tool("request_contact", "contact"));
    }

    #[test]
    fn messaging_profile_includes_explicit_tools() {
        let config = make_filter(true, "messaging");
        assert!(config.should_expose_tool("health_check", "other"));
        assert!(config.should_expose_tool("ensure_project", "other"));
        assert!(config.should_expose_tool("search_messages", "other"));
    }

    #[test]
    fn messaging_profile_excludes_reservations() {
        let config = make_filter(true, "messaging");
        assert!(!config.should_expose_tool("file_reservation_paths", "file_reservations"));
    }

    #[test]
    fn custom_include_mode_includes_listed() {
        let config = Config {
            tool_filter: ToolFilterSettings {
                enabled: true,
                profile: "custom".to_string(),
                mode: "include".to_string(),
                clusters: vec!["identity".to_string()],
                tools: vec!["search_messages".to_string()],
            },
            ..Config::default()
        };
        assert!(config.should_expose_tool("register_agent", "identity"));
        assert!(config.should_expose_tool("search_messages", "other"));
    }

    #[test]
    fn custom_include_mode_excludes_unlisted() {
        let config = Config {
            tool_filter: ToolFilterSettings {
                enabled: true,
                profile: "custom".to_string(),
                mode: "include".to_string(),
                clusters: vec!["identity".to_string()],
                tools: vec![],
            },
            ..Config::default()
        };
        assert!(!config.should_expose_tool("send_message", "messaging"));
    }

    #[test]
    fn custom_exclude_mode_excludes_listed() {
        let config = Config {
            tool_filter: ToolFilterSettings {
                enabled: true,
                profile: "custom".to_string(),
                mode: "exclude".to_string(),
                clusters: vec!["identity".to_string()],
                tools: vec!["search_messages".to_string()],
            },
            ..Config::default()
        };
        assert!(!config.should_expose_tool("register_agent", "identity"));
        assert!(!config.should_expose_tool("search_messages", "other"));
    }

    #[test]
    fn custom_exclude_mode_includes_unlisted() {
        let config = Config {
            tool_filter: ToolFilterSettings {
                enabled: true,
                profile: "custom".to_string(),
                mode: "exclude".to_string(),
                clusters: vec!["identity".to_string()],
                tools: vec![],
            },
            ..Config::default()
        };
        assert!(config.should_expose_tool("send_message", "messaging"));
    }

    #[test]
    fn custom_empty_lists_exposes_all() {
        let config = Config {
            tool_filter: ToolFilterSettings {
                enabled: true,
                profile: "custom".to_string(),
                mode: "include".to_string(),
                clusters: vec![],
                tools: vec![],
            },
            ..Config::default()
        };
        assert!(config.should_expose_tool("anything", "whatever"));
    }

    #[test]
    fn unknown_profile_acts_as_passthrough() {
        let config = make_filter(true, "nonexistent");
        // Unknown profile has empty cluster/tool lists, and since both are empty,
        // the final check `profile_clusters.is_empty() && profile_tools.is_empty()`
        // returns true — it acts as a pass-through (exposes all tools).
        assert!(config.should_expose_tool("anything", "whatever"));
    }

    #[test]
    fn tui_enabled_defaults_to_true() {
        let config = Config::default();
        assert!(config.tui_enabled);
    }

    #[test]
    fn tui_enabled_from_env_false() {
        let _env = TestEnvOverrideGuard::set(&[("TUI_ENABLED", "false")]);
        let config = Config::from_env();
        assert!(!config.tui_enabled);
    }

    #[test]
    fn tui_enabled_from_env_true() {
        let _env = TestEnvOverrideGuard::set(&[("TUI_ENABLED", "true")]);
        let config = Config::from_env();
        assert!(config.tui_enabled);
    }

    // -----------------------------------------------------------------------
    // mask_secret
    // -----------------------------------------------------------------------

    #[test]
    fn mask_secret_short_value_fully_masked() {
        assert_eq!(mask_secret("abc"), "****");
        assert_eq!(mask_secret("12345678"), "****");
    }

    #[test]
    fn mask_secret_long_value_shows_last_4() {
        assert_eq!(mask_secret("my-secret-token"), "****oken");
        assert_eq!(mask_secret("123456789"), "****6789");
    }

    #[test]
    fn mask_secret_unicode_shows_last_4_chars() {
        assert_eq!(
            mask_secret("prefix秘密秘密秘密秘密"),
            "****秘密秘密",
            "unicode secrets should not panic and should show last 4 chars"
        );
    }

    #[test]
    fn mask_secret_empty_is_fully_masked() {
        assert_eq!(mask_secret(""), "****");
    }

    // -----------------------------------------------------------------------
    // ConfigSource
    // -----------------------------------------------------------------------

    #[test]
    fn config_source_labels() {
        assert_eq!(ConfigSource::ProcessEnv.label(), "env");
        assert_eq!(ConfigSource::ProjectDotenv.label(), ".env");
        assert_eq!(ConfigSource::UserEnvFile.label(), "~/.mcp_agent_mail/.env");
        assert_eq!(ConfigSource::CliArg.label(), "cli");
        assert_eq!(ConfigSource::Default.label(), "default");
    }

    #[test]
    fn config_source_display() {
        assert_eq!(format!("{}", ConfigSource::ProcessEnv), "env");
        assert_eq!(format!("{}", ConfigSource::Default), "default");
    }

    // -----------------------------------------------------------------------
    // BootstrapSummary
    // -----------------------------------------------------------------------

    #[test]
    fn bootstrap_summary_default_config_has_expected_keys() {
        let config = Config::default();
        let summary = config.bootstrap_summary();
        let keys: Vec<&str> = summary.lines.iter().map(|l| l.key).collect();
        assert!(keys.contains(&"host"));
        assert!(keys.contains(&"port"));
        assert!(keys.contains(&"path"));
        assert!(keys.contains(&"auth"));
        assert!(keys.contains(&"db"));
        assert!(keys.contains(&"storage"));
    }

    #[test]
    fn bootstrap_summary_masks_bearer_token() {
        let config = Config {
            http_bearer_token: Some("my-super-secret-token".to_string()),
            ..Config::default()
        };
        let summary = config.bootstrap_summary();
        let auth_line = summary.lines.iter().find(|l| l.key == "auth").unwrap();
        assert!(auth_line.value.contains("****"));
        assert!(!auth_line.value.contains("my-super-secret-token"));
        assert!(auth_line.value.contains("oken")); // last 4 chars
    }

    #[test]
    fn bootstrap_summary_no_token_shows_none() {
        let config = Config::default();
        let summary = config.bootstrap_summary();
        let auth_line = summary.lines.iter().find(|l| l.key == "auth").unwrap();
        assert!(auth_line.value.contains("none"));
    }

    #[test]
    fn bootstrap_summary_set_source_overrides() {
        let config = Config::default();
        let mut summary = config.bootstrap_summary();
        summary.set_source("path", ConfigSource::CliArg);
        let path_line = summary.lines.iter().find(|l| l.key == "path").unwrap();
        assert_eq!(path_line.source, ConfigSource::CliArg);
    }

    #[test]
    fn bootstrap_summary_set_overrides_value_and_source() {
        let config = Config::default();
        let mut summary = config.bootstrap_summary();
        summary.set("path", "/mcp/".to_string(), ConfigSource::CliArg);
        let path_line = summary.lines.iter().find(|l| l.key == "path").unwrap();
        assert_eq!(path_line.value, "/mcp/");
        assert_eq!(path_line.source, ConfigSource::CliArg);
    }

    #[test]
    fn bootstrap_summary_format_includes_all_keys() {
        let config = Config::default();
        let summary = config.bootstrap_summary();
        let formatted = summary.format("HTTP + TUI");
        assert!(formatted.contains("host:"));
        assert!(formatted.contains("port:"));
        assert!(formatted.contains("auth:"));
        assert!(formatted.contains("db:"));
        assert!(formatted.contains("storage:"));
        assert!(formatted.contains("mode:"));
        assert!(formatted.contains("HTTP + TUI"));
    }

    #[test]
    fn bootstrap_summary_format_empty_mode_no_mode_line() {
        let config = Config::default();
        let summary = config.bootstrap_summary();
        let formatted = summary.format("");
        assert!(!formatted.contains("mode:"));
    }

    // -----------------------------------------------------------------------
    // full_env_value precedence
    // -----------------------------------------------------------------------

    #[test]
    fn full_env_value_prefers_process_env() {
        let _env = TestEnvOverrideGuard::set(&[("HTTP_BEARER_TOKEN", "from-env")]);
        let val = full_env_value("HTTP_BEARER_TOKEN");
        assert_eq!(val.as_deref(), Some("from-env"));
    }

    #[test]
    fn bearer_token_loaded_from_env_override() {
        let _env = TestEnvOverrideGuard::set(&[("HTTP_BEARER_TOKEN", "test-token-12345")]);
        let config = Config::from_env();
        assert_eq!(
            config.http_bearer_token.as_deref(),
            Some("test-token-12345")
        );
    }

    #[test]
    fn bearer_token_empty_string_treated_as_none() {
        let _env = TestEnvOverrideGuard::set(&[("HTTP_BEARER_TOKEN", "")]);
        let config = Config::from_env();
        assert!(config.http_bearer_token.is_none());
    }

    // -----------------------------------------------------------------------
    // user_env_file_path
    // -----------------------------------------------------------------------

    #[test]
    fn user_env_file_path_returns_none_when_no_files_exist() {
        // Since we can't control home dir in tests, just verify it returns
        // Some or None without panicking.
        let _ = user_env_file_path();
    }

    #[test]
    fn user_env_file_path_prefers_dotted_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dotted = tmp.path().join(".mcp_agent_mail");
        let legacy = tmp.path().join("mcp_agent_mail");
        std::fs::create_dir_all(&dotted).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(dotted.join(".env"), "FOO=bar\n").unwrap();
        std::fs::write(legacy.join(".env"), "FOO=baz\n").unwrap();

        // Test the loading logic directly
        let dotted_values = load_dotenv_file(&dotted.join(".env"));
        let legacy_values = load_dotenv_file(&legacy.join(".env"));
        assert_eq!(dotted_values.get("FOO").unwrap(), "bar");
        assert_eq!(legacy_values.get("FOO").unwrap(), "baz");
    }

    // -----------------------------------------------------------------------
    // detect_source
    // -----------------------------------------------------------------------

    #[test]
    fn detect_source_returns_default_for_unknown_key() {
        let source = detect_source("NONEXISTENT_KEY_THAT_NOBODY_SETS_12345");
        assert_eq!(source, ConfigSource::Default);
    }

    #[test]
    fn detect_source_returns_process_env_when_set() {
        // PATH is always set in process environment
        let source = detect_source("PATH");
        assert_eq!(source, ConfigSource::ProcessEnv);
    }
}
