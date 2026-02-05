//! Configuration management for MCP Agent Mail
//!
//! Configuration is loaded from environment variables, matching the legacy Python
//! implementation's python-decouple pattern.

use std::env;
use std::path::PathBuf;

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
    pub database_pool_size: usize,
    pub database_max_overflow: usize,
    pub database_pool_timeout: u64,

    // Storage
    pub storage_root: PathBuf,
    pub git_author_name: String,
    pub git_author_email: String,
    pub inline_image_max_bytes: usize,
    pub convert_images: bool,
    pub keep_original_images: bool,

    // HTTP
    pub http_host: String,
    pub http_port: u16,
    pub http_path: String,
    pub http_bearer_token: Option<String>,
    pub http_allow_localhost_unauthenticated: bool,

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
    pub messaging_auto_register_recipients: bool,
    pub messaging_auto_handshake_on_block: bool,

    // File Reservations
    pub file_reservations_cleanup_enabled: bool,
    pub file_reservations_cleanup_interval_seconds: u64,
    pub file_reservation_inactivity_seconds: u64,
    pub file_reservation_activity_grace_seconds: u64,
    pub file_reservations_enforcement_enabled: bool,

    // Agent Naming
    pub agent_name_enforcement_mode: AgentNameEnforcementMode,

    // LLM
    pub llm_enabled: bool,
    pub llm_default_model: String,
    pub llm_temperature: f64,
    pub llm_max_tokens: u32,
    pub llm_cache_enabled: bool,

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

    // TOON output format
    pub toon_bin: Option<String>,
    pub toon_stats_enabled: bool,
    pub output_format_default: Option<String>,

    // Logging
    pub log_level: String,
    pub log_rich_enabled: bool,
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
            database_pool_size: 3,
            database_max_overflow: 4,
            database_pool_timeout: 45,

            // Storage
            storage_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".mcp_agent_mail_git_mailbox_repo"),
            git_author_name: "mcp-agent".to_string(),
            git_author_email: "mcp-agent@example.com".to_string(),
            inline_image_max_bytes: 65536,
            convert_images: true,
            keep_original_images: false,

            // HTTP
            http_host: "127.0.0.1".to_string(),
            http_port: 8765,
            http_path: "/api/".to_string(),
            http_bearer_token: None,
            http_allow_localhost_unauthenticated: true,

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
            messaging_auto_register_recipients: true,
            messaging_auto_handshake_on_block: true,

            // File Reservations
            file_reservations_cleanup_enabled: false,
            file_reservations_cleanup_interval_seconds: 60,
            file_reservation_inactivity_seconds: 1800, // 30 minutes
            file_reservation_activity_grace_seconds: 900, // 15 minutes
            file_reservations_enforcement_enabled: true,

            // Agent Naming
            agent_name_enforcement_mode: AgentNameEnforcementMode::Coerce,

            // LLM
            llm_enabled: true,
            llm_default_model: "gpt-4o-mini".to_string(),
            llm_temperature: 0.2,
            llm_max_tokens: 512,
            llm_cache_enabled: true,

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

            // TOON output format
            toon_bin: None,
            toon_stats_enabled: false,
            output_format_default: None,

            // Logging
            log_level: "INFO".to_string(),
            log_rich_enabled: true,
        }
    }
}

impl Config {
    fn apply_environment_defaults(&mut self) {
        self.http_cors_enabled = self.app_environment == AppEnvironment::Development;
    }

    /// Load configuration from environment variables
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // Application
        if let Ok(v) = env::var("APP_ENVIRONMENT") {
            config.app_environment = match v.to_lowercase().as_str() {
                "production" | "prod" => AppEnvironment::Production,
                _ => AppEnvironment::Development,
            };
        }
        // Align CORS default with legacy behavior: enabled in development, disabled in production.
        config.apply_environment_defaults();
        config.worktrees_enabled = env_bool("WORKTREES_ENABLED", config.worktrees_enabled);
        if let Ok(v) = env::var("PROJECT_IDENTITY_MODE") {
            config.project_identity_mode = match v.to_lowercase().as_str() {
                "git-remote" => ProjectIdentityMode::GitRemote,
                "git-common-dir" => ProjectIdentityMode::GitCommonDir,
                "git-toplevel" => ProjectIdentityMode::GitToplevel,
                _ => ProjectIdentityMode::Dir,
            };
        }
        if let Ok(v) = env::var("PROJECT_IDENTITY_REMOTE") {
            config.project_identity_remote = v;
        }

        // Database
        if let Ok(v) = env::var("DATABASE_URL") {
            config.database_url = v;
        }
        config.database_echo = env_bool("DATABASE_ECHO", config.database_echo);
        config.database_pool_size = env_usize("DATABASE_POOL_SIZE", config.database_pool_size);
        config.database_max_overflow =
            env_usize("DATABASE_MAX_OVERFLOW", config.database_max_overflow);
        config.database_pool_timeout =
            env_u64("DATABASE_POOL_TIMEOUT", config.database_pool_timeout);

        // Storage
        if let Ok(v) = env::var("STORAGE_ROOT") {
            config.storage_root = PathBuf::from(shellexpand::tilde(&v).into_owned());
        }
        if let Ok(v) = env::var("GIT_AUTHOR_NAME") {
            config.git_author_name = v;
        }
        if let Ok(v) = env::var("GIT_AUTHOR_EMAIL") {
            config.git_author_email = v;
        }
        config.inline_image_max_bytes =
            env_usize("INLINE_IMAGE_MAX_BYTES", config.inline_image_max_bytes);
        config.convert_images = env_bool("CONVERT_IMAGES", config.convert_images);
        config.keep_original_images = env_bool("KEEP_ORIGINAL_IMAGES", config.keep_original_images);

        // HTTP
        if let Ok(v) = env::var("HTTP_HOST") {
            config.http_host = v;
        }
        config.http_port = env_u16("HTTP_PORT", config.http_port);
        if let Ok(v) = env::var("HTTP_PATH") {
            config.http_path = v;
        }
        config.http_bearer_token = env::var("HTTP_BEARER_TOKEN").ok().filter(|s| !s.is_empty());
        config.http_allow_localhost_unauthenticated = env_bool(
            "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED",
            config.http_allow_localhost_unauthenticated,
        );

        // Rate Limiting
        config.http_rate_limit_enabled =
            env_bool("HTTP_RATE_LIMIT_ENABLED", config.http_rate_limit_enabled);
        if let Ok(v) = env::var("HTTP_RATE_LIMIT_BACKEND") {
            config.http_rate_limit_backend = match v.to_lowercase().as_str() {
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
        config.http_rate_limit_redis_url = env::var("HTTP_RATE_LIMIT_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty());

        // JWT
        config.http_jwt_enabled = env_bool("HTTP_JWT_ENABLED", config.http_jwt_enabled);
        if let Ok(v) = env::var("HTTP_JWT_ALGORITHMS") {
            config.http_jwt_algorithms = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        config.http_jwt_secret = env::var("HTTP_JWT_SECRET").ok().filter(|s| !s.is_empty());
        config.http_jwt_jwks_url = env::var("HTTP_JWT_JWKS_URL").ok().filter(|s| !s.is_empty());
        config.http_jwt_audience = env::var("HTTP_JWT_AUDIENCE").ok().filter(|s| !s.is_empty());
        config.http_jwt_issuer = env::var("HTTP_JWT_ISSUER").ok().filter(|s| !s.is_empty());
        if let Ok(v) = env::var("HTTP_JWT_ROLE_CLAIM") {
            config.http_jwt_role_claim = v;
        }

        // RBAC
        config.http_rbac_enabled = env_bool("HTTP_RBAC_ENABLED", config.http_rbac_enabled);
        if let Ok(v) = env::var("HTTP_RBAC_READER_ROLES") {
            config.http_rbac_reader_roles = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = env::var("HTTP_RBAC_WRITER_ROLES") {
            config.http_rbac_writer_roles = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = env::var("HTTP_RBAC_DEFAULT_ROLE") {
            config.http_rbac_default_role = v;
        }
        if let Ok(v) = env::var("HTTP_RBAC_READONLY_TOOLS") {
            config.http_rbac_readonly_tools = v.split(',').map(|s| s.trim().to_string()).collect();
        }

        // CORS
        config.http_cors_enabled = env_bool("HTTP_CORS_ENABLED", config.http_cors_enabled);
        if let Ok(v) = env::var("HTTP_CORS_ORIGINS") {
            config.http_cors_origins = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        config.http_cors_allow_credentials = env_bool(
            "HTTP_CORS_ALLOW_CREDENTIALS",
            config.http_cors_allow_credentials,
        );
        if let Ok(v) = env::var("HTTP_CORS_ALLOW_METHODS") {
            let parsed = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            config.http_cors_allow_methods = if parsed.is_empty() {
                vec!["*".to_string()]
            } else {
                parsed
            };
        }
        if let Ok(v) = env::var("HTTP_CORS_ALLOW_HEADERS") {
            let parsed = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            config.http_cors_allow_headers = if parsed.is_empty() {
                vec!["*".to_string()]
            } else {
                parsed
            };
        }

        // Contact & Messaging
        config.contact_enforcement_enabled = env_bool(
            "CONTACT_ENFORCEMENT_ENABLED",
            config.contact_enforcement_enabled,
        );
        config.contact_auto_ttl_seconds =
            env_u64("CONTACT_AUTO_TTL_SECONDS", config.contact_auto_ttl_seconds);
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

        // Agent Naming
        if let Ok(v) = env::var("AGENT_NAME_ENFORCEMENT_MODE") {
            config.agent_name_enforcement_mode = match v.to_lowercase().as_str() {
                "strict" => AgentNameEnforcementMode::Strict,
                "always_auto" | "alwaysauto" => AgentNameEnforcementMode::AlwaysAuto,
                _ => AgentNameEnforcementMode::Coerce,
            };
        }

        // LLM
        config.llm_enabled = env_bool("LLM_ENABLED", config.llm_enabled);
        if let Ok(v) = env::var("LLM_DEFAULT_MODEL") {
            config.llm_default_model = v;
        }
        config.llm_temperature = env_f64("LLM_TEMPERATURE", config.llm_temperature);
        config.llm_max_tokens = env_u32("LLM_MAX_TOKENS", config.llm_max_tokens);
        config.llm_cache_enabled = env_bool("LLM_CACHE_ENABLED", config.llm_cache_enabled);

        // Notifications
        config.notifications_enabled =
            env_bool("NOTIFICATIONS_ENABLED", config.notifications_enabled);
        if let Ok(v) = env::var("NOTIFICATIONS_SIGNALS_DIR") {
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

        // Tool filtering
        config.tool_filter.enabled = env_bool("TOOLS_FILTER_ENABLED", config.tool_filter.enabled);
        if let Ok(v) = env::var("TOOLS_FILTER_PROFILE") {
            config.tool_filter.profile = normalize_tool_filter_profile(&v);
        }
        if let Ok(v) = env::var("TOOLS_FILTER_MODE") {
            config.tool_filter.mode = normalize_tool_filter_mode(&v);
        }
        if let Ok(v) = env::var("TOOLS_FILTER_CLUSTERS") {
            config.tool_filter.clusters = parse_csv_lower(&v);
        }
        if let Ok(v) = env::var("TOOLS_FILTER_TOOLS") {
            config.tool_filter.tools = parse_csv_lower(&v);
        }

        // TOON output format
        // Encoder binary: TOON_TRU_BIN > TOON_BIN > None (will use default "tru")
        config.toon_bin = env::var("TOON_TRU_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| env::var("TOON_BIN").ok().filter(|s| !s.is_empty()));
        config.toon_stats_enabled = env_bool("TOON_STATS", config.toon_stats_enabled);
        // Output format default: MCP_AGENT_MAIL_OUTPUT_FORMAT > TOON_DEFAULT_FORMAT > None
        config.output_format_default = env::var("MCP_AGENT_MAIL_OUTPUT_FORMAT")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env::var("TOON_DEFAULT_FORMAT")
                    .ok()
                    .filter(|s| !s.is_empty())
            });

        // Logging
        if let Ok(v) = env::var("LOG_LEVEL") {
            config.log_level = v;
        }
        config.log_rich_enabled = env_bool("LOG_RICH_ENABLED", config.log_rich_enabled);

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

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key).map_or(default, |v| {
        matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on")
    })
}

fn env_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_csv_lower(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
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
    env::var(key)
        .ok()
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
        assert_eq!(config.database_pool_size, 3);
        assert_eq!(
            config.database_url,
            "sqlite+aiosqlite:///./storage.sqlite3".to_string()
        );
        assert!(config.contact_enforcement_enabled);
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
}
