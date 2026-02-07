//! Connection pool configuration and initialization
//!
//! Uses `sqlmodel_pool` for efficient connection management.

use crate::error::{DbError, DbResult};
use crate::schema;
use asupersync::sync::OnceCell;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::{LockLevel, OrderedMutex, config::env_value};
use sqlmodel_core::Error as SqlError;
use sqlmodel_pool::{Pool, PoolConfig, PooledConnection};
use sqlmodel_sqlite::SqliteConnection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Default pool configuration values — sized for 1000+ concurrent agents.
///
/// ## Sizing rationale
///
/// `SQLite` WAL mode allows unlimited concurrent readers but serializes writers.
/// With a 1000-agent workload where ~10% are active simultaneously (~100 concurrent
/// tool calls) and a 3:1 read:write ratio, we need:
///
/// - **Readers**: At least 50 connections so read-heavy tools (`fetch_inbox`,
///   `search_messages`, resources) never queue behind writes.
/// - **Writers**: Only one writer executes at a time in WAL, so extra write
///   connections just queue on the WAL lock — but having a handful avoids
///   pool-acquire contention for the write path.
///
/// Defaults: `min=25, max=100`.  The pool lazily opens connections (starting from
/// `min`), so a lightly-loaded server uses only ~25 connections.  Under load the
/// pool grows up to 100, which still stays well within `SQLite` practical limits.
///
/// ## Timeout
///
/// Reduced from legacy 60s to 15s: if a connection isn't available within 15s the
/// circuit breaker should handle the failure rather than having the caller hang.
///
/// Override via `DATABASE_POOL_SIZE` / `DATABASE_MAX_OVERFLOW` env vars.
pub const DEFAULT_POOL_SIZE: usize = 25;
pub const DEFAULT_MAX_OVERFLOW: usize = 75;
pub const DEFAULT_POOL_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_POOL_RECYCLE_MS: u64 = 30 * 60 * 1000; // 30 minutes

/// Auto-detect a reasonable pool size from available CPU parallelism.
///
/// Returns `(min_connections, max_connections)`.  The heuristic is:
///
/// - `min = clamp(cpus * 4, 10, 50)`  — enough idle connections for moderate load
/// - `max = clamp(cpus * 12, 50, 200)` — headroom for burst traffic
///
/// This is used when `DATABASE_POOL_SIZE=auto` (the default when no explicit size
/// is given).
#[must_use]
pub fn auto_pool_size() -> (usize, usize) {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let min = (cpus * 4).clamp(10, 50);
    let max = (cpus * 12).clamp(50, 200);
    (min, max)
}

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
    /// Create config from environment.
    ///
    /// Pool sizing honours three strategies in priority order:
    ///
    /// 1. **Explicit**: `DATABASE_POOL_SIZE` and/or `DATABASE_MAX_OVERFLOW` are set
    ///    to numeric values → use those literally.
    /// 2. **Auto** (default): `DATABASE_POOL_SIZE` is unset or `"auto"` →
    ///    [`auto_pool_size()`] picks sizes based on CPU count.
    /// 3. **Legacy**: Set `DATABASE_POOL_SIZE=3` and `DATABASE_MAX_OVERFLOW=4` to
    ///    restore the legacy Python defaults (not recommended for production).
    #[must_use]
    pub fn from_env() -> Self {
        let database_url =
            env_value("DATABASE_URL").unwrap_or_else(|| "sqlite:///./storage.sqlite3".to_string());

        let pool_timeout = env_value("DATABASE_POOL_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POOL_TIMEOUT_MS);

        // Determine pool sizing: explicit, auto, or default constants.
        let pool_size_raw = env_value("DATABASE_POOL_SIZE");
        let explicit_size = pool_size_raw
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok());
        let explicit_overflow = env_value("DATABASE_MAX_OVERFLOW")
            .and_then(|s| s.parse::<usize>().ok());

        let (min_conn, max_conn) = match (explicit_size, explicit_overflow) {
            // Both explicitly set → honour literally.
            (Some(size), Some(overflow)) => (size, size + overflow),
            // Only size set → derive overflow from size.
            (Some(size), None) => (size, size.saturating_mul(4).max(size + DEFAULT_MAX_OVERFLOW)),
            // Not set, or explicitly "auto" → detect from hardware.
            (None, maybe_overflow) => {
                let (auto_min, auto_max) = auto_pool_size();
                match maybe_overflow {
                    Some(overflow) => (auto_min, auto_min + overflow),
                    None => (auto_min, auto_max),
                }
            }
        };

        Self {
            database_url,
            min_connections: min_conn,
            max_connections: max_conn,
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

#[derive(Debug)]
struct DbPoolStatsSampler {
    last_sample_us: AtomicU64,
    last_peak_reset_us: AtomicU64,
}

impl DbPoolStatsSampler {
    const SAMPLE_INTERVAL_US: u64 = 250_000; // 250ms
    const PEAK_WINDOW_US: u64 = 60_000_000; // 60s

    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_sample_us: AtomicU64::new(0),
            last_peak_reset_us: AtomicU64::new(0),
        }
    }

    pub fn sample_now(&self, pool: &Pool<SqliteConnection>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, true);
    }

    pub fn maybe_sample(&self, pool: &Pool<SqliteConnection>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, false);
    }

    fn sample_inner(&self, pool: &Pool<SqliteConnection>, now_us: u64, force: bool) {
        if force {
            self.last_sample_us.store(now_us, Ordering::Relaxed);
        } else {
            let last = self.last_sample_us.load(Ordering::Relaxed);
            if now_us.saturating_sub(last) < Self::SAMPLE_INTERVAL_US {
                return;
            }
            if self
                .last_sample_us
                .compare_exchange(last, now_us, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
        }

        let stats = pool.stats();
        let metrics = mcp_agent_mail_core::global_metrics();

        let total = u64::try_from(stats.total_connections).unwrap_or(u64::MAX);
        let idle = u64::try_from(stats.idle_connections).unwrap_or(u64::MAX);
        let active = u64::try_from(stats.active_connections).unwrap_or(u64::MAX);
        let pending = u64::try_from(stats.pending_requests).unwrap_or(u64::MAX);

        metrics.db.pool_total_connections.set(total);
        metrics.db.pool_idle_connections.set(idle);
        metrics.db.pool_active_connections.set(active);
        metrics.db.pool_pending_requests.set(pending);

        // Peak is a rolling 60s high-water mark (best-effort; updated on sampling).
        let reset_last = self.last_peak_reset_us.load(Ordering::Relaxed);
        if (reset_last == 0 || now_us.saturating_sub(reset_last) >= Self::PEAK_WINDOW_US)
            && self
                .last_peak_reset_us
                .compare_exchange(reset_last, now_us, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            metrics.db.pool_peak_active_connections.set(active);
        }
        metrics.db.pool_peak_active_connections.fetch_max(active);

        // Track "pool has been >= 80% utilized" duration (in micros since epoch).
        let util_pct = if total == 0 {
            0
        } else {
            active.saturating_mul(100).saturating_div(total)
        };
        if util_pct >= 80 {
            if metrics.db.pool_over_80_since_us.load() == 0 {
                metrics.db.pool_over_80_since_us.set(now_us);
            }
        } else {
            metrics.db.pool_over_80_since_us.set(0);
        }
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
    run_migrations: bool,
    stats_sampler: Arc<DbPoolStatsSampler>,
}

impl DbPool {
    /// Create a new pool (does not open connections until first acquire).
    pub fn new(config: &DbPoolConfig) -> DbResult<Self> {
        let sqlite_path = config.sqlite_path()?;
        let init_sql = Arc::new(schema::PRAGMA_CONN_SETTINGS_SQL.to_string());
        let stats_sampler = Arc::new(DbPoolStatsSampler::new());

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
            run_migrations: config.run_migrations,
            stats_sampler,
        })
    }

    #[must_use]
    pub fn sqlite_path(&self) -> &str {
        &self.sqlite_path
    }

    pub fn sample_pool_stats_now(&self) {
        self.stats_sampler.sample_now(&self.pool);
    }

    /// Acquire a pooled connection, creating and initializing a new one if needed.
    #[allow(clippy::too_many_lines)]
    pub async fn acquire(&self, cx: &Cx) -> Outcome<PooledConnection<SqliteConnection>, SqlError> {
        let sqlite_path = self.sqlite_path.clone();
        let init_sql = self.init_sql.clone();
        let run_migrations = self.run_migrations;
        let cx2 = cx.clone();

        let start = Instant::now();
        let out = self
            .pool
            .acquire(cx, || {
                let sqlite_path = sqlite_path.clone();
                let init_sql = init_sql.clone();
                let cx2 = cx2.clone();
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

                    // Per-connection PRAGMAs matching legacy Python `db.py` event listeners.
                    if let Err(e) = conn.execute_raw(&init_sql) {
                        return Outcome::Err(e);
                    }

                    if sqlite_path == ":memory:" {
                        // In-memory DB connections do not share state. Each connection must run
                        // migrations independently.
                        if run_migrations {
                            match schema::migrate_to_latest(&cx2, &conn).await {
                                Outcome::Ok(_) => {}
                                Outcome::Err(e) => return Outcome::Err(e),
                                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                                Outcome::Panicked(p) => return Outcome::Panicked(p),
                            }
                        }
                    } else {
                        // File-backed DB: apply DB-wide initialization exactly once per sqlite file.
                        // This avoids high-concurrency races where multiple new connections try to
                        // set WAL mode and/or run migrations simultaneously.
                        let init_gate = sqlite_init_gate(&sqlite_path);
                        let run_migrations = run_migrations;
                        let conn_ref = &conn;

                        let gate_out = init_gate
                            .get_or_try_init(|| {
                                let cx2 = cx2.clone();
                                async move {
                                    if let Err(e) = conn_ref.execute_raw(schema::PRAGMA_DB_INIT_SQL)
                                    {
                                        return Err(Outcome::Err(e));
                                    }
                                    if run_migrations {
                                        match schema::migrate_to_latest(&cx2, conn_ref).await {
                                            Outcome::Ok(_) => {}
                                            Outcome::Err(e) => return Err(Outcome::Err(e)),
                                            Outcome::Cancelled(r) => {
                                                return Err(Outcome::Cancelled(r));
                                            }
                                            Outcome::Panicked(p) => {
                                                return Err(Outcome::Panicked(p));
                                            }
                                        }
                                    }
                                    Ok(())
                                }
                            })
                            .await;

                        match gate_out {
                            Ok(()) => {}
                            Err(Outcome::Err(e)) => return Outcome::Err(e),
                            Err(Outcome::Cancelled(r)) => return Outcome::Cancelled(r),
                            Err(Outcome::Panicked(p)) => return Outcome::Panicked(p),
                            Err(Outcome::Ok(())) => {
                                unreachable!("sqlite init gate returned Err(Outcome::Ok(()))")
                            }
                        }
                    }

                    Outcome::Ok(conn)
                }
            })
            .await;

        let dur_us = u64::try_from(start.elapsed().as_micros().min(u128::from(u64::MAX)))
            .unwrap_or(u64::MAX);
        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.db.pool_acquires_total.inc();
        metrics.db.pool_acquire_latency_us.record(dur_us);
        if !matches!(out, Outcome::Ok(_)) {
            metrics.db.pool_acquire_errors_total.inc();
        }

        // Best-effort sampling for pool utilization gauges (bounded frequency).
        self.stats_sampler.maybe_sample(&self.pool);

        out
    }
}

static SQLITE_INIT_GATES: OnceLock<OrderedMutex<HashMap<String, Arc<OnceCell<()>>>>> =
    OnceLock::new();
static POOL_CACHE: OnceLock<OrderedMutex<HashMap<String, DbPool>>> = OnceLock::new();

fn sqlite_init_gate(sqlite_path: &str) -> Arc<OnceCell<()>> {
    let gates = SQLITE_INIT_GATES
        .get_or_init(|| OrderedMutex::new(LockLevel::DbSqliteInitGates, HashMap::new()));
    let mut guard = gates.lock();
    if let Some(gate) = guard.get(sqlite_path) {
        return Arc::clone(gate);
    }
    let gate = Arc::new(OnceCell::new());
    guard.insert(sqlite_path.to_string(), Arc::clone(&gate));
    gate
}

/// Get (or create) a cached pool for the given config.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let cache =
        POOL_CACHE.get_or_init(|| OrderedMutex::new(LockLevel::DbPoolCache, HashMap::new()));
    let mut guard = cache.lock();

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

    /// Verify pool defaults are sized for 1000+ concurrent agent workloads.
    ///
    /// The defaults were upgraded from the legacy Python values (3+4=7) to
    /// support high concurrency: min=25, max=100.
    #[test]
    fn pool_defaults_sized_for_scale() {
        assert_eq!(DEFAULT_POOL_SIZE, 25, "min connections for scale");
        assert_eq!(DEFAULT_MAX_OVERFLOW, 75, "overflow headroom for bursts");
        assert_eq!(
            DEFAULT_POOL_TIMEOUT_MS, 15_000,
            "15s timeout (fail fast, let circuit breaker handle)"
        );
        assert_eq!(
            DEFAULT_POOL_RECYCLE_MS,
            30 * 60 * 1000,
            "pool_recycle is 1800s (30 min)"
        );

        let cfg = DbPoolConfig::default();
        assert_eq!(cfg.min_connections, 25);
        assert_eq!(cfg.max_connections, 100); // 25 + 75
        assert_eq!(cfg.max_lifetime_ms, 1_800_000); // 30 min in ms
    }

    /// Verify auto-sizing picks reasonable values based on CPU count.
    #[test]
    fn auto_pool_size_is_reasonable() {
        let (min, max) = auto_pool_size();
        // Must be within configured clamp bounds.
        assert!(min >= 10 && min <= 50, "auto min={min} should be in [10, 50]");
        assert!(max >= 50 && max <= 200, "auto max={max} should be in [50, 200]");
        assert!(max >= min, "max must be >= min");
        // On a 4-core machine: min=16, max=48→50.  On 16-core: min=50, max=192.
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        assert_eq!(min, (cpus * 4).clamp(10, 50));
        assert_eq!(max, (cpus * 12).clamp(50, 200));
    }

    /// Verify PRAGMA settings contain `busy_timeout=60000` matching legacy Python.
    #[test]
    fn pragma_busy_timeout_matches_legacy() {
        let sql = schema::init_schema_sql();
        let busy_idx = sql
            .find("busy_timeout = 60000")
            .expect("schema init sql must contain busy_timeout");
        let wal_idx = sql
            .find("journal_mode = WAL")
            .expect("schema init sql must contain journal_mode=WAL");
        assert!(
            busy_idx < wal_idx,
            "busy_timeout must be set before journal_mode to avoid SQLITE_BUSY before timeout applies"
        );
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
