//! Configuration management for MCP Agent Mail
//!
//! Configuration is loaded from environment variables, matching the legacy Python
//! implementation's python-decouple pattern.

use std::collections::HashMap;
use std::env;
use std::fs;
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

    // Storage
    pub storage_root: PathBuf,
    pub git_author_name: String,
    pub git_author_email: String,
    pub inline_image_max_bytes: usize,
    pub convert_images: bool,
    pub keep_original_images: bool,
    pub allow_absolute_attachment_paths: bool,

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
    pub log_include_trace: bool,
    pub log_json_enabled: bool,
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
            log_include_trace: false,
            log_json_enabled: false,
        }
    }
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

        // HTTP
        if let Some(v) = env_value("HTTP_HOST") {
            config.http_host = v;
        }
        config.http_port = env_u16("HTTP_PORT", config.http_port);
        if let Some(v) = env_value("HTTP_PATH") {
            config.http_path = v;
        }
        config.http_bearer_token = env_value("HTTP_BEARER_TOKEN").filter(|s| !s.is_empty());
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
        config.log_include_trace = env_bool("LOG_INCLUDE_TRACE", config.log_include_trace);
        config.log_json_enabled = env_bool("LOG_JSON_ENABLED", config.log_json_enabled);

        config
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
}

// Helper functions for environment variable parsing

static DOTENV_VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();

fn dotenv_values() -> &'static HashMap<String, String> {
    DOTENV_VALUES.get_or_init(|| load_dotenv_file(Path::new(".env")))
}

/// Read a value from the .env file (if present).
#[must_use]
pub fn dotenv_value(key: &str) -> Option<String> {
    dotenv_values().get(key).cloned()
}

/// Read a value from the real environment first, falling back to .env.
#[must_use]
pub fn env_value(key: &str) -> Option<String> {
    env::var(key).ok().or_else(|| dotenv_value(key))
}

fn load_dotenv_file(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_dotenv_contents(&contents)
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
}
