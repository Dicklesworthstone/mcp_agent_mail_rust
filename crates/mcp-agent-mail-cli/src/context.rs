//! Shared CLI command framework: context, resolution, and formatting helpers.
//!
//! Every CLI handler that touches the database should go through [`CliContext`]
//! (sync, for read-only queries) or [`AsyncCliContext`] (async, for write
//! operations that need the full storage layer).
//!
//! This module provides:
//! - **`CliContext`** — sync DB connection + config bundle
//! - **`AsyncCliContext`** — async pool + config bundle for write paths
//! - **Project / agent resolution** — look up IDs from user-supplied keys
//! - **Timestamp formatting** — human-friendly display helpers
//! - **`resolve_bool`** — canonical `--flag` / `--no-flag` resolution

#![forbid(unsafe_code)]

use mcp_agent_mail_core::{Config, InterfaceMode};
use mcp_agent_mail_db::DbPoolConfig;

use crate::{CliError, CliResult};

// ── Sync context ────────────────────────────────────────────────────────

/// Shared context for sync CLI handlers (read-only queries).
pub struct CliContext {
    pub conn: mcp_agent_mail_db::DbConn,
    pub config: Config,
}

impl CliContext {
    /// Open a sync context using the default env-based config.
    pub fn open() -> CliResult<Self> {
        let mut config = Config::from_env();
        config.interface_mode = InterfaceMode::Cli;
        let pool_cfg = DbPoolConfig::from_env();
        let conn = open_conn(&pool_cfg)?;
        Ok(Self { conn, config })
    }

    /// Open with a specific database URL (for testing / overrides).
    pub fn open_with_url(database_url: &str) -> CliResult<Self> {
        let mut config = Config::from_env();
        config.interface_mode = InterfaceMode::Cli;
        let pool_cfg = DbPoolConfig {
            database_url: database_url.to_string(),
            ..Default::default()
        };
        let conn = open_conn(&pool_cfg)?;
        Ok(Self { conn, config })
    }

    /// Resolve a project slug or human_key to `(project_id, slug)`.
    pub fn resolve_project(&self, key: &str) -> CliResult<ResolvedProject> {
        resolve_project(&self.conn, key)
    }

    /// Resolve an agent name within a project to its agent ID.
    pub fn resolve_agent(&self, project_id: i64, agent_name: &str) -> CliResult<ResolvedAgent> {
        resolve_agent(&self.conn, project_id, agent_name)
    }
}

// ── Async context ───────────────────────────────────────────────────────

/// Shared context for async CLI handlers (write paths via storage layer).
pub struct AsyncCliContext {
    pub pool: mcp_agent_mail_db::DbPool,
    pub config: Config,
}

impl AsyncCliContext {
    /// Create an async context using the default env-based config.
    pub fn open() -> CliResult<Self> {
        let mut config = Config::from_env();
        config.interface_mode = InterfaceMode::Cli;
        let pool_cfg = DbPoolConfig::from_env();
        let pool = mcp_agent_mail_db::get_or_create_pool(&pool_cfg)
            .map_err(|e| CliError::Other(format!("db pool init failed: {e}")))?;
        Ok(Self { pool, config })
    }

    /// Build an MCP server URL from config, for server-tool delegation.
    pub fn server_url(&self) -> String {
        format!(
            "http://{}:{}{}",
            self.config.http_host, self.config.http_port, self.config.http_path
        )
    }

    /// Get the bearer token from config (if set).
    pub fn bearer(&self) -> Option<&str> {
        self.config.http_bearer_token.as_deref()
    }
}

// ── Project resolution ──────────────────────────────────────────────────

/// A resolved project identity.
#[derive(Debug, Clone)]
pub struct ResolvedProject {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
}

/// Look up a project by slug or `human_key`.
///
/// Tries slug match first, then `human_key` match.
pub fn resolve_project(
    conn: &mcp_agent_mail_db::DbConn,
    key: &str,
) -> CliResult<ResolvedProject> {
    // Try slug first
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects WHERE slug = ? LIMIT 1",
            &[sqlmodel_core::Value::Text(key.to_string())],
        )
        .map_err(|e| CliError::Other(format!("project query failed: {e}")))?;

    if let Some(row) = rows.first() {
        return Ok(ResolvedProject {
            id: row.get_named("id").unwrap_or(0),
            slug: row.get_named("slug").unwrap_or_default(),
            human_key: row.get_named("human_key").unwrap_or_default(),
        });
    }

    // Try human_key
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects WHERE human_key = ? LIMIT 1",
            &[sqlmodel_core::Value::Text(key.to_string())],
        )
        .map_err(|e| CliError::Other(format!("project query failed: {e}")))?;

    if let Some(row) = rows.first() {
        return Ok(ResolvedProject {
            id: row.get_named("id").unwrap_or(0),
            slug: row.get_named("slug").unwrap_or_default(),
            human_key: row.get_named("human_key").unwrap_or_default(),
        });
    }

    Err(CliError::InvalidArgument(format!(
        "project not found: {key}"
    )))
}

// ── Agent resolution ────────────────────────────────────────────────────

/// A resolved agent identity.
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub id: i64,
    pub name: String,
    pub project_id: i64,
}

/// Look up an agent by name within a project.
pub fn resolve_agent(
    conn: &mcp_agent_mail_db::DbConn,
    project_id: i64,
    agent_name: &str,
) -> CliResult<ResolvedAgent> {
    let rows = conn
        .query_sync(
            "SELECT id, name FROM agents WHERE project_id = ? AND name = ? LIMIT 1",
            &[
                sqlmodel_core::Value::BigInt(project_id),
                sqlmodel_core::Value::Text(agent_name.to_string()),
            ],
        )
        .map_err(|e| CliError::Other(format!("agent query failed: {e}")))?;

    if let Some(row) = rows.first() {
        return Ok(ResolvedAgent {
            id: row.get_named("id").unwrap_or(0),
            name: row.get_named("name").unwrap_or_default(),
            project_id,
        });
    }

    Err(CliError::InvalidArgument(format!(
        "agent not found: {agent_name}"
    )))
}

// ── Timestamp formatting ────────────────────────────────────────────────

/// Format a microsecond timestamp as a human-friendly ISO-8601 string.
///
/// Returns `"--"` for zero/sentinel values to keep table output clean.
pub fn format_ts(micros: i64) -> String {
    if micros == 0 {
        return "--".to_string();
    }
    mcp_agent_mail_db::timestamps::micros_to_iso(micros)
}

/// Format a microsecond timestamp as a short human-readable string.
///
/// Returns `"2026-02-08 15:30"` style — date + time to minutes.
/// Returns `"--"` for zero/sentinel values.
pub fn format_ts_short(micros: i64) -> String {
    if micros == 0 {
        return "--".to_string();
    }
    let secs = micros.div_euclid(1_000_000);
    let nanos = (micros.rem_euclid(1_000_000) * 1000) as u32;
    let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) else {
        return format_ts(micros);
    };
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Format a duration in seconds as a human-readable string.
///
/// Returns strings like `"2h 15m"`, `"45m"`, `"30s"`, `"< 1s"`.
pub fn format_duration(seconds: i64) -> String {
    if seconds < 1 {
        return "< 1s".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_min = minutes % 60;
    if rem_min == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rem_min}m")
    }
}

// ── Boolean flag resolution ─────────────────────────────────────────────

/// Canonical `--flag` / `--no-flag` resolution for clap bool pairs.
///
/// `primary` is the affirmative flag, `negated` is the `--no-*` flag.
/// When neither is set, `default` is returned.
pub fn resolve_bool(primary: bool, negated: bool, default: bool) -> bool {
    if negated {
        return false;
    }
    if primary {
        return true;
    }
    default
}

// ── Internal helpers ────────────────────────────────────────────────────

fn open_conn(
    cfg: &DbPoolConfig,
) -> CliResult<mcp_agent_mail_db::DbConn> {
    let path = cfg
        .sqlite_path()
        .map_err(|e| CliError::Other(format!("bad database URL: {e}")))?;
    let conn = mcp_agent_mail_db::DbConn::open_file(&path)
        .map_err(|e| CliError::Other(format!("cannot open DB at {path}: {e}")))?;
    let init_sql = mcp_agent_mail_db::schema::init_schema_sql_base();
    conn.execute_raw(&init_sql)
        .map_err(|e| CliError::Other(format!("schema init failed: {e}")))?;
    Ok(conn)
}

// ── Async runtime helper ────────────────────────────────────────────────

/// Run an async closure in a single-threaded runtime.
///
/// Use this to bridge from sync `handle_*()` to async storage operations:
/// ```ignore
/// pub fn handle_foo(args: FooArgs) -> CliResult<()> {
///     run_async(async move {
///         let ctx = AsyncCliContext::open()?;
///         // ... async work ...
///         Ok(())
///     })
/// }
/// ```
pub fn run_async<F, T>(future: F) -> CliResult<T>
where
    F: std::future::Future<Output = CliResult<T>>,
{
    use asupersync::runtime::RuntimeBuilder;

    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build runtime: {e}")))?;
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_ts ───────────────────────────────────────────────────────

    #[test]
    fn format_ts_zero_returns_dash() {
        assert_eq!(format_ts(0), "--");
    }

    #[test]
    fn format_ts_positive_returns_iso() {
        let micros = 1_707_400_000_000_000; // ~2024-02-08
        let result = format_ts(micros);
        assert!(result.contains("2024"), "expected 2024 in {result}");
    }

    // ── format_ts_short ─────────────────────────────────────────────────

    #[test]
    fn format_ts_short_zero_returns_dash() {
        assert_eq!(format_ts_short(0), "--");
    }

    #[test]
    fn format_ts_short_positive_returns_short_format() {
        let micros = 1_707_400_000_000_000; // ~2024-02-08
        let result = format_ts_short(micros);
        assert!(result.contains("2024-02-08"), "expected date in {result}");
        // Should NOT contain seconds or timezone
        assert!(!result.contains('+'), "should not have tz offset: {result}");
    }

    // ── format_duration ─────────────────────────────────────────────────

    #[test]
    fn format_duration_sub_second() {
        assert_eq!(format_duration(0), "< 1s");
        assert_eq!(format_duration(-5), "< 1s");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3599), "59m");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(7200), "2h");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(3660), "1h 1m");
        assert_eq!(format_duration(8100), "2h 15m");
    }

    // ── resolve_bool ────────────────────────────────────────────────────

    #[test]
    fn resolve_bool_default_when_neither_set() {
        assert!(resolve_bool(false, false, true));
        assert!(!resolve_bool(false, false, false));
    }

    #[test]
    fn resolve_bool_primary_wins() {
        assert!(resolve_bool(true, false, false));
    }

    #[test]
    fn resolve_bool_negated_wins_over_default() {
        assert!(!resolve_bool(false, true, true));
    }

    #[test]
    fn resolve_bool_negated_wins_over_primary() {
        // Edge case: both set — negated takes precedence
        assert!(!resolve_bool(true, true, true));
    }

    // ── CliContext ───────────────────────────────────────────────────────

    #[test]
    fn cli_context_open_with_temp_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        // Should be able to query (tables exist from schema init)
        let rows = ctx
            .conn
            .query_sync("SELECT COUNT(*) AS cnt FROM projects", &[])
            .unwrap();
        let cnt: i64 = rows.first().unwrap().get_named("cnt").unwrap();
        assert_eq!(cnt, 0);
    }

    #[test]
    fn resolve_project_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        let err = ctx.resolve_project("nonexistent").unwrap_err();
        assert!(
            err.to_string().contains("project not found"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn resolve_project_by_slug() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        // Insert a project
        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('test-proj', '/tmp/test', 1000000)",
            )
            .unwrap();

        let proj = ctx.resolve_project("test-proj").unwrap();
        assert_eq!(proj.slug, "test-proj");
        assert_eq!(proj.human_key, "/tmp/test");
        assert!(proj.id > 0);
    }

    #[test]
    fn resolve_project_by_human_key() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('my-slug', '/data/myproj', 1000000)",
            )
            .unwrap();

        let proj = ctx.resolve_project("/data/myproj").unwrap();
        assert_eq!(proj.slug, "my-slug");
        assert_eq!(proj.human_key, "/data/myproj");
    }

    #[test]
    fn resolve_agent_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('p', '/tmp/p', 1000000)",
            )
            .unwrap();
        let proj = ctx.resolve_project("p").unwrap();

        let err = ctx.resolve_agent(proj.id, "NoSuchAgent").unwrap_err();
        assert!(
            err.to_string().contains("agent not found"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn resolve_agent_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('p', '/tmp/p', 1000000)",
            )
            .unwrap();
        let proj = ctx.resolve_project("p").unwrap();

        ctx.conn
            .execute_raw(&format!(
                "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy) \
                 VALUES ({}, 'RedFox', 'test', 'test-model', '', 1000000, 1000000, 'auto')",
                proj.id
            ))
            .unwrap();

        let agent = ctx.resolve_agent(proj.id, "RedFox").unwrap();
        assert_eq!(agent.name, "RedFox");
        assert_eq!(agent.project_id, proj.id);
        assert!(agent.id > 0);
    }
}
