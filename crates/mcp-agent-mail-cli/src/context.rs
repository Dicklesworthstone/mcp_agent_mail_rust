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

use std::path::Path;

use mcp_agent_mail_core::{Config, InterfaceMode, resolve_project_identity};
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
            normalize_client_connect_host(&self.config.http_host),
            self.config.http_port,
            self.config.http_path
        )
    }

    /// Get the bearer token from config (if set).
    pub fn bearer(&self) -> Option<&str> {
        self.config.http_bearer_token.as_deref()
    }
}

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

// ── Project resolution ──────────────────────────────────────────────────

/// A resolved project identity.
#[derive(Debug, Clone)]
pub struct ResolvedProject {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub created_at: i64,
}

fn orphaned_project_placeholder(project_id: i64, created_at: i64) -> ResolvedProject {
    let placeholder = format!("[unknown-project-{project_id}]");
    ResolvedProject {
        id: project_id,
        slug: placeholder.clone(),
        human_key: placeholder,
        created_at,
    }
}

fn query_project_by_slug(
    conn: &mcp_agent_mail_db::DbConn,
    slug: &str,
) -> CliResult<Option<ResolvedProject>> {
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects WHERE slug = ? COLLATE NOCASE LIMIT 1",
            &[sqlmodel_core::Value::Text(slug.to_string())],
        )
        .map_err(|e| CliError::Other(format!("project query failed: {e}")))?;

    rows.first()
        .map(|row| {
            Ok(ResolvedProject {
                id: require_i64_column(row, "id", "projects")?,
                slug: require_text_column(row, "slug", "projects")?,
                human_key: require_text_column(row, "human_key", "projects")?,
                created_at: 0,
            })
        })
        .transpose()
}

fn query_project_by_human_key(
    conn: &mcp_agent_mail_db::DbConn,
    human_key: &str,
) -> CliResult<Option<ResolvedProject>> {
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key FROM projects WHERE human_key = ? LIMIT 1",
            &[sqlmodel_core::Value::Text(human_key.to_string())],
        )
        .map_err(|e| CliError::Other(format!("project query failed: {e}")))?;

    rows.first()
        .map(|row| {
            Ok(ResolvedProject {
                id: require_i64_column(row, "id", "projects")?,
                slug: require_text_column(row, "slug", "projects")?,
                human_key: require_text_column(row, "human_key", "projects")?,
                created_at: 0,
            })
        })
        .transpose()
}

fn list_project_inventory(conn: &mcp_agent_mail_db::DbConn) -> CliResult<Vec<ResolvedProject>> {
    let project_rows = conn
        .query_sync(
            "SELECT id, slug, human_key, created_at FROM projects ORDER BY created_at ASC, id ASC",
            &[],
        )
        .map_err(|e| CliError::Other(format!("project inventory query failed: {e}")))?;
    let mut out = Vec::with_capacity(project_rows.len());
    for row in project_rows {
        out.push(ResolvedProject {
            id: require_i64_column(&row, "id", "projects")?,
            slug: require_text_column(&row, "slug", "projects")?,
            human_key: require_text_column(&row, "human_key", "projects")?,
            created_at: require_i64_column(&row, "created_at", "projects")?,
        });
    }

    let now_us = mcp_agent_mail_core::timestamps::now_micros();
    let orphan_sources: Vec<(String, Vec<sqlmodel_core::Value>)> = vec![
        (
            "SELECT DISTINCT m.project_id AS project_id \
             FROM messages m \
             LEFT JOIN projects p ON p.id = m.project_id \
             WHERE p.id IS NULL"
                .to_string(),
            vec![],
        ),
        (
            "SELECT DISTINCT a.project_id AS project_id \
             FROM agents a \
             LEFT JOIN projects p ON p.id = a.project_id \
             WHERE p.id IS NULL"
                .to_string(),
            vec![],
        ),
        (
            format!(
                "SELECT DISTINCT file_reservations.project_id AS project_id \
                 FROM file_reservations \
                 LEFT JOIN projects p ON p.id = file_reservations.project_id \
                 WHERE p.id IS NULL \
                   AND ({}) \
                   AND file_reservations.expires_ts > ?",
                mcp_agent_mail_db::queries::ACTIVE_RESERVATION_PREDICATE
            ),
            vec![sqlmodel_core::Value::BigInt(now_us)],
        ),
        (
            "SELECT DISTINCT ppl.project_id AS project_id \
             FROM product_project_links ppl \
             LEFT JOIN projects p ON p.id = ppl.project_id \
             WHERE p.id IS NULL"
                .to_string(),
            vec![],
        ),
    ];
    let mut orphan_project_ids = std::collections::BTreeSet::new();
    for (sql, params) in orphan_sources {
        let rows = conn
            .query_sync(&sql, &params)
            .map_err(|e| CliError::Other(format!("project inventory query failed: {e}")))?;
        for row in rows {
            if let Ok(project_id) = row.get_named::<i64>("project_id") {
                orphan_project_ids.insert(project_id);
            }
        }
    }

    let orphan_created_at_sql = format!(
        "SELECT COALESCE( \
             (SELECT MIN(m.created_ts) FROM messages m WHERE m.project_id = ?), \
             (SELECT MIN(a.inception_ts) FROM agents a WHERE a.project_id = ?), \
             (SELECT MIN(fr.created_ts) FROM file_reservations fr \
              WHERE fr.project_id = ? \
                AND ({}) \
                AND fr.expires_ts > ?), \
             (SELECT MIN(ppl.created_at) FROM product_project_links ppl WHERE ppl.project_id = ?), \
             0 \
         ) AS created_at",
        mcp_agent_mail_db::queries::active_reservation_predicate_for("fr")
    );
    for project_id in orphan_project_ids {
        let created_at_rows = conn
            .query_sync(
                &orphan_created_at_sql,
                &[
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(project_id),
                    sqlmodel_core::Value::BigInt(now_us),
                    sqlmodel_core::Value::BigInt(project_id),
                ],
            )
            .map_err(|e| CliError::Other(format!("project inventory query failed: {e}")))?;
        let created_at = created_at_rows
            .first()
            .and_then(|row| row.get_named::<i64>("created_at").ok())
            .unwrap_or(0);
        out.push(orphaned_project_placeholder(project_id, created_at));
    }

    out.sort_by_key(|project| (project.created_at, project.id));
    Ok(out)
}

fn project_matches_absolute_lookup(
    project: &ResolvedProject,
    requested: &mcp_agent_mail_core::ProjectIdentity,
) -> bool {
    let stored = resolve_project_identity(&project.human_key);
    stored.human_key == requested.human_key || stored.canonical_path == requested.canonical_path
}

/// Look up a project by slug or `human_key`.
///
/// For absolute filesystem paths, prefer exact/canonical `human_key` matching
/// before accepting a slug hit so slug collisions cannot resolve to the wrong
/// project.
pub fn resolve_project(conn: &mcp_agent_mail_db::DbConn, key: &str) -> CliResult<ResolvedProject> {
    let key = key.trim();
    if Path::new(key).is_absolute() {
        let requested = resolve_project_identity(key);
        if let Some(project) = query_project_by_human_key(conn, &requested.human_key)? {
            return Ok(project);
        }
        if requested.human_key != key
            && let Some(project) = query_project_by_human_key(conn, key)?
        {
            return Ok(project);
        }
        if let Some(project) = query_project_by_slug(conn, &requested.slug)?
            && project_matches_absolute_lookup(&project, &requested)
        {
            return Ok(project);
        }
    } else {
        if let Some(project) = query_project_by_slug(conn, key)? {
            return Ok(project);
        }
        if let Some(project) = query_project_by_human_key(conn, key)? {
            return Ok(project);
        }
        if let Some(project) = list_project_inventory(conn)?
            .into_iter()
            .find(|project| project.slug.eq_ignore_ascii_case(key) || project.human_key == key)
        {
            return Ok(project);
        }
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
///
/// Agent names are resolved case-insensitively when the project has a single
/// matching row. Legacy databases may still contain case-duplicate rows before
/// `am migrate`; in that case, fail instead of guessing which agent was meant.
pub fn resolve_agent(
    conn: &mcp_agent_mail_db::DbConn,
    project_id: i64,
    agent_name: &str,
) -> CliResult<ResolvedAgent> {
    let rows = conn
        .query_sync(
            "SELECT id, name FROM agents \
             WHERE project_id = ? AND name = ? COLLATE NOCASE \
             ORDER BY id ASC LIMIT 2",
            &[
                sqlmodel_core::Value::BigInt(project_id),
                sqlmodel_core::Value::Text(agent_name.to_string()),
            ],
        )
        .map_err(|e| CliError::Other(format!("agent query failed: {e}")))?;

    if rows.len() > 1 {
        return Err(CliError::InvalidArgument(format!(
            "ambiguous agent name '{agent_name}' in project {project_id}; run `am migrate` to deduplicate legacy case-duplicate rows"
        )));
    }

    if let Some(row) = rows.first() {
        return Ok(ResolvedAgent {
            id: require_i64_column(row, "id", "agents")?,
            name: require_text_column(row, "name", "agents")?,
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

fn open_conn(cfg: &DbPoolConfig) -> CliResult<mcp_agent_mail_db::DbConn> {
    crate::open_db_sync_with_database_url(&cfg.database_url)
}

fn require_i64_column(
    row: &sqlmodel_core::Row,
    column: &'static str,
    table: &'static str,
) -> CliResult<i64> {
    row.get_named(column).map_err(|_| {
        CliError::Other(format!(
            "invalid {table} row: missing or non-integer `{column}` column"
        ))
    })
}

fn require_text_column(
    row: &sqlmodel_core::Row,
    column: &'static str,
    table: &'static str,
) -> CliResult<String> {
    row.get_named(column).map_err(|_| {
        CliError::Other(format!(
            "invalid {table} row: missing or non-text `{column}` column"
        ))
    })
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

    #[test]
    fn normalize_client_connect_host_maps_wildcards_and_ipv6() {
        assert_eq!(normalize_client_connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(normalize_client_connect_host("::"), "[::1]");
        assert_eq!(
            normalize_client_connect_host("2001:db8::42"),
            "[2001:db8::42]"
        );
        assert_eq!(normalize_client_connect_host("[::1]"), "[::1]");
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
    fn resolve_project_by_slug_falls_back_to_orphaned_placeholder_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('my-slug', '/data/myproj', 1000000)",
            )
            .unwrap();
        let project_id = ctx.resolve_project("my-slug").unwrap().id;
        ctx.conn
            .execute_raw(&format!(
                "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy) \
                 VALUES ({project_id}, 'RedFox', 'test', 'test-model', '', 1000000, 1000000, 'auto')",
            ))
            .unwrap();
        ctx.conn
            .execute_raw(&format!("DELETE FROM projects WHERE id = {project_id}"))
            .unwrap();

        let placeholder = format!("[unknown-project-{project_id}]");
        let proj = ctx.resolve_project(&placeholder).unwrap();
        assert_eq!(proj.id, project_id);
        assert_eq!(proj.slug, placeholder);
        assert_eq!(proj.human_key, format!("[unknown-project-{project_id}]"));
    }

    #[test]
    fn resolve_project_by_human_key_falls_back_to_orphaned_placeholder_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        ctx.conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('my-slug', '/data/myproj', 1000000)",
            )
            .unwrap();
        let project_id = ctx.resolve_project("my-slug").unwrap().id;
        ctx.conn
            .execute_raw(
                "INSERT INTO products (id, product_uid, name, created_at) VALUES (3, 'prod-3', 'prod-3', 1000000)",
            )
            .unwrap();
        ctx.conn
            .execute_raw(&format!(
                "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (3, {project_id}, 1000000)"
            ))
            .unwrap();
        ctx.conn
            .execute_raw(&format!("DELETE FROM projects WHERE id = {project_id}"))
            .unwrap();

        let placeholder = format!("[unknown-project-{project_id}]");
        let proj = ctx.resolve_project(&placeholder).unwrap();
        assert_eq!(proj.id, project_id);
        assert_eq!(proj.slug, placeholder);
        assert_eq!(proj.human_key, format!("[unknown-project-{project_id}]"));
    }

    #[test]
    fn resolve_project_rejects_slug_collision_for_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        let project_a = dir.path().join("repo").join("a-b");
        let project_b = dir.path().join("repo").join("a").join("b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();

        let project_a = project_a.canonicalize().unwrap();
        let project_b = project_b.canonicalize().unwrap();
        let project_a_key = project_a.display().to_string();
        let project_b_key = project_b.display().to_string();

        let identity_a = resolve_project_identity(&project_a_key);
        let identity_b = resolve_project_identity(&project_b_key);
        assert_eq!(
            identity_a.slug, identity_b.slug,
            "test setup requires a slug collision"
        );

        ctx.conn
            .execute_raw(&format!(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('{}', '{}', 1000000)",
                identity_a.slug, project_a_key
            ))
            .unwrap();

        let err = ctx.resolve_project(&project_b_key).unwrap_err();
        assert!(
            err.to_string().contains("project not found"),
            "unexpected: {err}"
        );
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

    #[test]
    fn resolve_agent_found_case_insensitive() {
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

        let agent = ctx.resolve_agent(proj.id, "redfox").unwrap();
        assert_eq!(agent.name, "RedFox");
        assert_eq!(agent.project_id, proj.id);
        assert!(agent.id > 0);
    }

    #[test]
    fn resolve_agent_rejects_ambiguous_case_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        let ctx = CliContext::open_with_url(&url).unwrap();

        // Recreate a legacy pre-migration duplicate state so the resolver's
        // ambiguity handling can be exercised on a migrated test database.
        ctx.conn
            .execute_raw("DROP INDEX IF EXISTS idx_agents_project_name_nocase")
            .unwrap();

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
        ctx.conn
            .execute_raw(&format!(
                "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy) \
                 VALUES ({}, 'redfox', 'test', 'test-model', '', 1000000, 1000000, 'auto')",
                proj.id
            ))
            .unwrap();

        let err = ctx.resolve_agent(proj.id, "ReDFoX").unwrap_err();
        assert!(
            err.to_string().contains("ambiguous agent name"),
            "unexpected: {err}"
        );
        assert!(err.to_string().contains("am migrate"), "unexpected: {err}");
    }
}
