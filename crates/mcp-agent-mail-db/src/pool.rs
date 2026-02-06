//! Connection pool configuration and initialization
//!
//! Uses `sqlmodel_pool` for efficient connection management.

use crate::error::{DbError, DbResult};
use crate::schema;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::config::env_value;
use sqlmodel_core::Error as SqlError;
use sqlmodel_pool::{Pool, PoolConfig, PooledConnection};
use sqlmodel_sqlite::SqliteConnection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Default pool configuration values (matching legacy Python `db.py`).
///
/// - `pool_size=3` + `max_overflow=4` = 7 max connections (`SQLite` default).
/// - WAL mode allows all 7 to read concurrently; writes serialize through the WAL
///   writer lock but the 60s `busy_timeout` (set in PRAGMAs) prevents `SQLITE_BUSY`.
/// - Pool recycle every 30 minutes (1800s) to prevent stale connections.
///
/// Override via `DATABASE_POOL_SIZE` / `DATABASE_MAX_OVERFLOW` env vars for higher load.
pub const DEFAULT_POOL_SIZE: usize = 3;
pub const DEFAULT_MAX_OVERFLOW: usize = 4;
pub const DEFAULT_POOL_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_POOL_RECYCLE_MS: u64 = 30 * 60 * 1000; // 30 minutes (legacy: pool_recycle=1800)

/// Pool configuration
#[derive(Debug, Clone)]
pub struct DbPoolConfig {
    /// Database URL (`sqlite:///path/to/db.sqlite3`)
    pub database_url: String,
    /// Minimum connections to keep open
    pub min_connections: usize,
    /// Maximum connections
    pub max_connections: usize,
    /// Timeout for acquiring a connection (ms)
    pub acquire_timeout_ms: u64,
    /// Max connection lifetime (ms)
    pub max_lifetime_ms: u64,
    /// Run migrations on init
    pub run_migrations: bool,
}

impl Default for DbPoolConfig {
    fn default() -> Self {
        Self {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            min_connections: DEFAULT_POOL_SIZE,
            max_connections: DEFAULT_POOL_SIZE + DEFAULT_MAX_OVERFLOW,
            acquire_timeout_ms: DEFAULT_POOL_TIMEOUT_MS,
            max_lifetime_ms: DEFAULT_POOL_RECYCLE_MS,
            run_migrations: true,
        }
    }
}

impl DbPoolConfig {
    /// Create config from environment
    #[must_use]
    pub fn from_env() -> Self {
        let database_url =
            env_value("DATABASE_URL").unwrap_or_else(|| "sqlite:///./storage.sqlite3".to_string());

        let pool_size = env_value("DATABASE_POOL_SIZE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POOL_SIZE);

        let max_overflow = env_value("DATABASE_MAX_OVERFLOW")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_OVERFLOW);

        let pool_timeout = env_value("DATABASE_POOL_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POOL_TIMEOUT_MS);

        Self {
            database_url,
            min_connections: pool_size,
            max_connections: pool_size + max_overflow,
            acquire_timeout_ms: pool_timeout,
            max_lifetime_ms: DEFAULT_POOL_RECYCLE_MS,
            run_migrations: true,
        }
    }

    /// Parse `SQLite` path from database URL
    pub fn sqlite_path(&self) -> DbResult<String> {
        // Handle various URL formats:
        // - sqlite:///./path.db
        // - sqlite:////absolute/path.db
        // - sqlite+aiosqlite:///./path.db (Python format)
        // - sqlite:///:memory: (in-memory)
        let url = self
            .database_url
            .trim_start_matches("sqlite+aiosqlite://")
            .trim_start_matches("sqlite://");

        if url.is_empty() {
            return Err(DbError::InvalidArgument {
                field: "database_url",
                message: "Empty database path".to_string(),
            });
        }

        // Special case for in-memory database
        if url == "/:memory:" {
            return Ok(":memory:".to_string());
        }

        // After stripping "sqlite://", the URL is like:
        // - /./path.db (relative) -> ./path.db
        // - //absolute/path.db (absolute) -> /absolute/path.db
        // - /path.db (might be relative or absolute) -> /path.db

        // Handle relative paths: /./path -> ./path
        if url.starts_with("/./") {
            return Ok(url[1..].to_string());
        }

        // Handle absolute paths: //path -> /path (double slash after sqlite://)
        if url.starts_with("//") {
            return Ok(url[1..].to_string());
        }

        // Single leading slash or bare path
        Ok(url.to_string())
    }
}

/// A configured `SQLite` connection pool with schema initialization.
///
/// This wraps `sqlmodel_pool::Pool<SqliteConnection>` and encapsulates:
/// - URL/path parsing (`sqlite+aiosqlite:///...` etc)
/// - per-connection PRAGMAs + schema init (idempotent)
#[derive(Clone)]
pub struct DbPool {
    pool: Arc<Pool<SqliteConnection>>,
    sqlite_path: String,
    init_sql: Arc<String>,
}

impl DbPool {
    /// Create a new pool (does not open connections until first acquire).
    pub fn new(config: &DbPoolConfig) -> DbResult<Self> {
        let sqlite_path = config.sqlite_path()?;
        let init_sql = Arc::new(schema::init_schema_sql());

        let pool_config = PoolConfig::new(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout_ms)
            .max_lifetime(config.max_lifetime_ms)
            // Legacy Python favors responsiveness; validate on checkout.
            .test_on_checkout(true)
            .test_on_return(false);

        Ok(Self {
            pool: Arc::new(Pool::new(pool_config)),
            sqlite_path,
            init_sql,
        })
    }

    #[must_use]
    pub fn sqlite_path(&self) -> &str {
        &self.sqlite_path
    }

    /// Acquire a pooled connection, creating and initializing a new one if needed.
    pub async fn acquire(&self, cx: &Cx) -> Outcome<PooledConnection<SqliteConnection>, SqlError> {
        let sqlite_path = self.sqlite_path.clone();
        let init_sql = self.init_sql.clone();

        self.pool
            .acquire(cx, || {
                let sqlite_path = sqlite_path.clone();
                let init_sql = init_sql.clone();
                async move {
                    // Ensure parent directory exists for file-backed DBs.
                    if sqlite_path != ":memory:" {
                        if let Some(parent) = Path::new(&sqlite_path).parent() {
                            if !parent.as_os_str().is_empty() {
                                if let Err(e) = std::fs::create_dir_all(parent) {
                                    return Outcome::Err(SqlError::Custom(format!(
                                        "failed to create db dir {}: {e}",
                                        parent.display()
                                    )));
                                }
                            }
                        }
                    }

                    let conn = if sqlite_path == ":memory:" {
                        match SqliteConnection::open_memory() {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    } else {
                        match SqliteConnection::open_file(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    };

                    // Idempotent schema init: PRAGMAs + CREATE TABLE/TRIGGER/FTS IF NOT EXISTS.
                    if let Err(e) = conn.execute_raw(&init_sql) {
                        return Outcome::Err(e);
                    }

                    Outcome::Ok(conn)
                }
            })
            .await
    }
}

static POOL_CACHE: OnceLock<Mutex<HashMap<String, DbPool>>> = OnceLock::new();

/// Get (or create) a cached pool for the given config.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let cache = POOL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Some(pool) = guard.get(&config.database_url) {
        return Ok(pool.clone());
    }

    let pool = DbPool::new(config)?;
    guard.insert(config.database_url.clone(), pool.clone());
    drop(guard);
    Ok(pool)
}

/// Create (or reuse) a pool for the given config.
///
/// This is kept for backwards compatibility with earlier skeleton code.
pub fn create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    get_or_create_pool(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_path_parsing() {
        let config = DbPoolConfig {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:////absolute/path/db.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/absolute/path/db.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite+aiosqlite:///./legacy.db".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./legacy.db");

        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");
    }

    #[test]
    fn test_schema_init_in_memory() {
        use sqlmodel_core::{Row, Value};
        use sqlmodel_sqlite::SqliteConnection;

        // Open in-memory connection
        let conn = SqliteConnection::open_memory().expect("failed to open in-memory db");

        // Get schema SQL
        let sql = schema::init_schema_sql();
        println!("Schema SQL length: {} bytes", sql.len());

        // Execute it
        conn.execute_raw(&sql).expect("failed to init schema");

        // Verify a table exists by querying it
        let rows: Vec<Row> = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
                &[],
            )
            .expect("failed to query tables");

        let table_names: Vec<String> = rows
            .iter()
            .filter_map(|r: &Row| {
                if let Some(Value::Text(s)) = r.get_by_name("name") {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();

        println!("Created tables: {table_names:?}");

        assert!(table_names.contains(&"projects".to_string()));
        assert!(table_names.contains(&"agents".to_string()));
        assert!(table_names.contains(&"messages".to_string()));
    }

    /// Verify pool defaults match legacy Python db.py values exactly.
    ///
    /// Legacy Python (`SQLite` mode):
    /// - `pool_size=3`, `max_overflow=4`, `pool_recycle=1800` (30 min)
    /// - `busy_timeout=60000` (60s)
    #[test]
    fn pool_defaults_match_legacy_python() {
        assert_eq!(DEFAULT_POOL_SIZE, 3, "legacy pool_size is 3 for SQLite");
        assert_eq!(
            DEFAULT_MAX_OVERFLOW, 4,
            "legacy max_overflow is 4 for SQLite"
        );
        assert_eq!(
            DEFAULT_POOL_RECYCLE_MS,
            30 * 60 * 1000,
            "legacy pool_recycle is 1800s (30 min)"
        );

        let cfg = DbPoolConfig::default();
        assert_eq!(cfg.min_connections, 3);
        assert_eq!(cfg.max_connections, 7); // 3 + 4
        assert_eq!(cfg.max_lifetime_ms, 1_800_000); // 30 min in ms
    }

    /// Verify PRAGMA settings contain `busy_timeout=60000` matching legacy Python.
    #[test]
    fn pragma_busy_timeout_matches_legacy() {
        let sql = schema::init_schema_sql();
        assert!(
            sql.contains("busy_timeout = 60000"),
            "PRAGMA busy_timeout must be 60000 (60s) to match Python legacy"
        );
        assert!(
            sql.contains("journal_mode = WAL"),
            "WAL mode is required for concurrent access"
        );
    }
}
