//! Connection pool configuration and initialization
//!
//! Uses `sqlmodel_pool` for efficient connection management.

use crate::DbConn;
use crate::error::{DbError, DbResult};
use crate::integrity;
use crate::schema;
use asupersync::sync::OnceCell;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::{
    ConsistencyMessageRef, LockLevel, OrderedRwLock,
    config::env_value,
    disk::{is_sqlite_memory_database_url, sqlite_file_path_from_database_url},
};
use sqlmodel_core::{Error as SqlError, Value};
use sqlmodel_pool::{Pool, PoolConfig, PooledConnection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

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
    let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
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
    /// Number of connections to eagerly open on startup (0 = disabled).
    /// Capped at `min_connections`. Warmup is bounded by `acquire_timeout_ms`.
    pub warmup_connections: usize,
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
            warmup_connections: 0,
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
        let explicit_overflow =
            env_value("DATABASE_MAX_OVERFLOW").and_then(|s| s.parse::<usize>().ok());

        let (min_conn, max_conn) = match (explicit_size, explicit_overflow) {
            // Both explicitly set → honour literally.
            (Some(size), Some(overflow)) => (size, size + overflow),
            // Only size set → derive overflow from size.
            (Some(size), None) => (
                size,
                size.saturating_mul(4).max(size + DEFAULT_MAX_OVERFLOW),
            ),
            // Not set, or explicitly "auto" → detect from hardware.
            (None, maybe_overflow) => {
                let (auto_min, auto_max) = auto_pool_size();
                maybe_overflow.map_or((auto_min, auto_max), |overflow| {
                    (auto_min, auto_min + overflow)
                })
            }
        };

        let warmup = env_value("DATABASE_POOL_WARMUP")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
            .min(min_conn);

        Self {
            database_url,
            min_connections: min_conn,
            max_connections: max_conn,
            acquire_timeout_ms: pool_timeout,
            max_lifetime_ms: DEFAULT_POOL_RECYCLE_MS,
            run_migrations: true,
            warmup_connections: warmup,
        }
    }

    /// Parse `SQLite` path from database URL
    pub fn sqlite_path(&self) -> DbResult<String> {
        if is_sqlite_memory_database_url(&self.database_url) {
            return Ok(":memory:".to_string());
        }

        let Some(path) = sqlite_file_path_from_database_url(&self.database_url) else {
            return Err(DbError::InvalidArgument {
                field: "database_url",
                message: format!(
                    "Invalid SQLite database URL: {} (expected sqlite:///path/to/db.sqlite3)",
                    self.database_url
                ),
            });
        };

        Ok(path.to_string_lossy().into_owned())
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

    pub fn sample_now(&self, pool: &Pool<DbConn>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, true);
    }

    pub fn maybe_sample(&self, pool: &Pool<DbConn>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, false);
    }

    fn sample_inner(&self, pool: &Pool<DbConn>, now_us: u64, force: bool) {
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
/// This wraps `sqlmodel_pool::Pool<DbConn>` and encapsulates:
/// - URL/path parsing (`sqlite+aiosqlite:///...` etc)
/// - per-connection PRAGMAs + schema init (idempotent)
#[derive(Clone)]
pub struct DbPool {
    pool: Arc<Pool<DbConn>>,
    sqlite_path: String,
    init_sql: Arc<String>,
    run_migrations: bool,
    stats_sampler: Arc<DbPoolStatsSampler>,
}

impl DbPool {
    /// Create a new pool (does not open connections until first acquire).
    pub fn new(config: &DbPoolConfig) -> DbResult<Self> {
        let sqlite_path = config.sqlite_path()?;
        let init_sql = Arc::new(schema::build_conn_pragmas(config.max_connections));
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
    pub async fn acquire(&self, cx: &Cx) -> Outcome<PooledConnection<DbConn>, SqlError> {
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

                    // For file-backed DBs, run DB-wide init (journal mode, migrations) with
                    // C-backed SqliteConnection BEFORE opening FrankenConnection.
                    // FrankenConnection and SqliteConnection must never be open on the
                    // same file simultaneously — concurrent access corrupts the database.
                    if sqlite_path != ":memory:" {
                        let init_gate = sqlite_init_gate(&sqlite_path);
                        let run_migrations = run_migrations;

                        let gate_out = init_gate
                            .get_or_try_init(|| {
                                let cx2 = cx2.clone();
                                let sqlite_path = sqlite_path.clone();
                                async move {
                                    // Use C-backed SqliteConnection for DB-wide init.
                                    //
                                    // IMPORTANT: Use base-safe DB init pragmas (no WAL mode)
                                    // and only run *base* migrations (no FTS5 virtual
                                    // tables, no triggers). FrankenConnection cannot open
                                    // database files that contain FTS5 shadow table pages
                                    // ("cell has no rowid" error). Search falls back to LIKE
                                    // automatically when FTS5 tables are absent.
                                    if let Err(e) =
                                        ensure_sqlite_file_healthy(Path::new(&sqlite_path))
                                    {
                                        return Err(Outcome::Err(e));
                                    }
                                    let mig_conn =
                                        sqlmodel_sqlite::SqliteConnection::open_file(&sqlite_path)
                                            .map_err(Outcome::<(), SqlError>::Err)?;

                                    if let Err(e) =
                                        mig_conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL)
                                    {
                                        return Err(Outcome::Err(e));
                                    }
                                    if run_migrations {
                                        match schema::migrate_to_latest_base(&cx2, &mig_conn).await
                                        {
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
                                    if let Err(e) = schema::enforce_base_mode_cleanup(&mig_conn) {
                                        return Err(Outcome::Err(e));
                                    }
                                    // Drop SqliteConnection before FrankenConnection opens.
                                    drop(mig_conn);
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

                    // Now open FrankenConnection — SqliteConnection is fully closed.
                    let conn = if sqlite_path == ":memory:" {
                        match DbConn::open_memory() {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    } else {
                        match DbConn::open_file(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    };

                    // Per-connection PRAGMAs matching legacy Python `db.py` event listeners.
                    if let Err(e) = conn.execute_raw(&init_sql) {
                        return Outcome::Err(e);
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

    /// Eagerly open up to `n` connections to avoid first-burst latency.
    ///
    /// Connections are acquired and immediately returned to the pool idle set.
    /// Bounded: stops after `timeout` elapses or on first acquire error.
    /// Returns the number of connections successfully warmed up.
    pub async fn warmup(&self, cx: &Cx, n: usize, timeout: std::time::Duration) -> usize {
        let deadline = Instant::now() + timeout;
        let mut opened = 0usize;
        // Acquire connections in batches; hold them briefly then release.
        let mut batch: Vec<PooledConnection<DbConn>> = Vec::with_capacity(n);
        for _ in 0..n {
            if Instant::now() >= deadline {
                break;
            }
            match self.acquire(cx).await {
                Outcome::Ok(conn) => {
                    batch.push(conn);
                    opened += 1;
                }
                _ => break, // stop on any error (timeout, cancelled, etc.)
            }
        }
        // Drop all connections back to idle pool
        drop(batch);
        opened
    }

    /// Run a `PRAGMA quick_check` on a fresh connection to validate database
    /// integrity at startup. Returns `Ok(result)` if healthy, or
    /// `Err(IntegrityCorruption)` if corruption is detected.
    ///
    /// This opens a dedicated connection (outside the pool) so the check
    /// doesn't consume a pooled slot.
    pub fn run_startup_integrity_check(&self) -> DbResult<integrity::IntegrityCheckResult> {
        if self.sqlite_path == ":memory:" {
            // In-memory databases cannot be corrupt on startup.
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Quick,
            });
        }

        // Check if the file exists first; a missing file is not corruption.
        if !Path::new(&self.sqlite_path).exists() {
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Quick,
            });
        }

        let conn = DbConn::open_file(&self.sqlite_path)
            .map_err(|e| DbError::Sqlite(format!("startup integrity check: open failed: {e}")))?;

        integrity::quick_check(&conn)
    }

    /// Run a full `PRAGMA integrity_check` on a dedicated connection.
    ///
    /// This can take seconds on large databases. Should be called from a
    /// background task, not from the request hot path.
    pub fn run_full_integrity_check(&self) -> DbResult<integrity::IntegrityCheckResult> {
        if self.sqlite_path == ":memory:" {
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Full,
            });
        }

        if !Path::new(&self.sqlite_path).exists() {
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Full,
            });
        }

        let conn = DbConn::open_file(&self.sqlite_path)
            .map_err(|e| DbError::Sqlite(format!("full integrity check: open failed: {e}")))?;

        integrity::full_check(&conn)
    }

    /// Sample the N most recent messages from the DB for consistency checking.
    ///
    /// Returns lightweight refs that the storage layer can use to verify
    /// archive file presence. Opens a dedicated connection (outside the pool)
    /// so this works even if the pool isn't fully started yet.
    pub fn sample_recent_message_refs(&self, limit: i64) -> DbResult<Vec<ConsistencyMessageRef>> {
        if self.sqlite_path == ":memory:" {
            return Ok(Vec::new());
        }
        if !Path::new(&self.sqlite_path).exists() {
            return Ok(Vec::new());
        }

        let conn = DbConn::open_file(&self.sqlite_path)
            .map_err(|e| DbError::Sqlite(format!("consistency probe: open failed: {e}")))?;

        // Query recent messages joined with projects and agents to get
        // the slug, sender name, subject, and created timestamp.
        let sql = "\
            SELECT m.id, p.slug, a.name AS sender_name, m.subject, m.created_ts \
            FROM messages m \
            JOIN projects p ON m.project_id = p.id \
            JOIN agents a ON m.sender_id = a.id \
            ORDER BY m.id DESC \
            LIMIT ?";

        let rows = conn
            .query_sync(sql, &[sqlmodel_core::Value::BigInt(limit)])
            .map_err(|e| DbError::Sqlite(format!("consistency probe query: {e}")))?;

        let mut refs = Vec::with_capacity(rows.len());
        for row in &rows {
            let id = match row.get_by_name("id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let slug = match row.get_by_name("slug") {
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let sender = match row.get_by_name("sender_name") {
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let subject = match row.get_by_name("subject") {
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let created_ts = match row.get_by_name("created_ts") {
                Some(sqlmodel_core::Value::BigInt(us)) => crate::micros_to_iso(*us),
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };

            refs.push(ConsistencyMessageRef {
                project_slug: slug,
                message_id: id,
                sender_name: sender,
                subject,
                created_ts_iso: created_ts,
            });
        }

        Ok(refs)
    }

    /// Run an explicit WAL checkpoint (`TRUNCATE` mode).
    ///
    /// This moves all WAL content back into the main database file and truncates
    /// the WAL to zero length. Useful for:
    /// - Graceful shutdown (ensures DB file is self-contained)
    /// - Before export/snapshot (no loose WAL journal)
    /// - Idle periods (reclaim WAL disk space)
    ///
    /// Returns the number of WAL frames checkpointed, or an error.
    /// No-ops silently for `:memory:` databases.
    pub fn wal_checkpoint(&self) -> DbResult<u64> {
        if self.sqlite_path == ":memory:" {
            return Ok(0);
        }
        let conn = DbConn::open_file(&self.sqlite_path)
            .map_err(|e| DbError::Sqlite(format!("checkpoint: open failed: {e}")))?;

        // Apply busy_timeout so the checkpoint waits for active readers/writers.
        conn.execute_raw("PRAGMA busy_timeout = 60000;")
            .map_err(|e| DbError::Sqlite(format!("checkpoint: busy_timeout: {e}")))?;

        let rows = conn
            .query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[])
            .map_err(|e| DbError::Sqlite(format!("checkpoint: {e}")))?;

        // wal_checkpoint returns (busy, log, checkpointed)
        let checkpointed = rows
            .first()
            .and_then(|r| match r.get_by_name("checkpointed") {
                Some(sqlmodel_core::Value::BigInt(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                Some(sqlmodel_core::Value::Int(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                _ => None,
            })
            .unwrap_or(0);

        Ok(checkpointed)
    }
}

static SQLITE_INIT_GATES: OnceLock<OrderedRwLock<HashMap<String, Arc<OnceCell<()>>>>> =
    OnceLock::new();
static POOL_CACHE: OnceLock<OrderedRwLock<HashMap<String, DbPool>>> = OnceLock::new();

fn sqlite_init_gate(sqlite_path: &str) -> Arc<OnceCell<()>> {
    let gates = SQLITE_INIT_GATES
        .get_or_init(|| OrderedRwLock::new(LockLevel::DbSqliteInitGates, HashMap::new()));

    // Fast path: read lock for existing gate (concurrent readers).
    {
        let guard = gates.read();
        if let Some(gate) = guard.get(sqlite_path) {
            return Arc::clone(gate);
        }
    }

    // Slow path: write lock to create a new gate (rare, once per SQLite file).
    let mut guard = gates.write();
    // Double-check after acquiring write lock.
    if let Some(gate) = guard.get(sqlite_path) {
        return Arc::clone(gate);
    }
    let gate = Arc::new(OnceCell::new());
    guard.insert(sqlite_path.to_string(), Arc::clone(&gate));
    gate
}

fn is_corruption_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("database disk image is malformed")
        || lower.contains("malformed database schema")
        || lower.contains("file is not a database")
}

fn sqlite_quick_check_is_ok(conn: &sqlmodel_sqlite::SqliteConnection) -> Result<bool, SqlError> {
    let rows = conn.query_sync("PRAGMA quick_check", &[])?;
    let mut details: Vec<String> = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Ok(v) = row.get_named::<String>("quick_check") {
            details.push(v);
        } else if let Ok(v) = row.get_named::<String>("integrity_check") {
            details.push(v);
        } else if let Some(Value::Text(v)) = row.values().next() {
            details.push(v.clone());
        }
    }
    if details.is_empty() {
        // Some backends may return an empty rowset for success.
        return Ok(true);
    }
    Ok(details.len() == 1 && details[0] == "ok")
}

fn sqlite_file_is_healthy(path: &Path) -> Result<bool, SqlError> {
    if !path.exists() {
        return Ok(true);
    }
    let path_str = path.to_string_lossy();
    let conn = match sqlmodel_sqlite::SqliteConnection::open_file(path_str.as_ref()) {
        Ok(conn) => conn,
        Err(e) => {
            if is_corruption_error_message(&e.to_string()) {
                return Ok(false);
            }
            return Err(e);
        }
    };

    match sqlite_quick_check_is_ok(&conn) {
        Ok(ok) => Ok(ok),
        Err(e) => {
            if is_corruption_error_message(&e.to_string()) {
                return Ok(false);
            }
            Err(e)
        }
    }
}

fn sqlite_backup_candidates(primary_path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<(u8, SystemTime, PathBuf)> = Vec::new();
    let Some(file_name) = primary_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let Some(parent) = primary_path.parent() else {
        return Vec::new();
    };

    let bak = primary_path.with_file_name(format!("{file_name}.bak"));
    if bak.is_file() {
        let modified = bak
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((0, modified, bak));
    }

    let backup_prefix = format!("{file_name}.backup-");
    let recovery_prefix = format!("{file_name}.recovery");
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let priority = if name.starts_with(&backup_prefix) {
                1
            } else if name.starts_with(&recovery_prefix) {
                2
            } else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((priority, modified, path));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    candidates.into_iter().map(|(_, _, p)| p).collect()
}

fn find_healthy_backup(primary_path: &Path) -> Option<PathBuf> {
    for candidate in sqlite_backup_candidates(primary_path) {
        match sqlite_file_is_healthy(&candidate) {
            Ok(true) => return Some(candidate),
            Ok(false) => tracing::warn!(
                candidate = %candidate.display(),
                "sqlite backup candidate failed quick_check; skipping"
            ),
            Err(e) => tracing::warn!(
                candidate = %candidate.display(),
                error = %e,
                "sqlite backup candidate unreadable; skipping"
            ),
        }
    }
    None
}

fn quarantine_sidecar(primary_path: &Path, suffix: &str, timestamp: &str) -> Result<(), SqlError> {
    let mut source_os = primary_path.as_os_str().to_os_string();
    source_os.push(suffix);
    let source = PathBuf::from(source_os);
    if !source.exists() {
        return Ok(());
    }
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let target = primary_path.with_file_name(format!("{base_name}{suffix}.corrupt-{timestamp}"));
    std::fs::rename(&source, &target).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine sidecar {}: {e}",
            source.display()
        ))
    })
}

fn restore_from_backup(primary_path: &Path, backup_path: &Path) -> Result<(), SqlError> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined_db = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));

    std::fs::rename(primary_path, &quarantined_db).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine corrupted database {}: {e}",
            primary_path.display()
        ))
    })?;

    if let Err(e) = quarantine_sidecar(primary_path, "-wal", &timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-wal", primary_path.display()),
            error = %e,
            "failed to quarantine WAL sidecar; continuing"
        );
    }
    if let Err(e) = quarantine_sidecar(primary_path, "-shm", &timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-shm", primary_path.display()),
            error = %e,
            "failed to quarantine SHM sidecar; continuing"
        );
    }

    if let Err(e) = std::fs::copy(backup_path, primary_path) {
        let _ = std::fs::rename(&quarantined_db, primary_path);
        return Err(SqlError::Custom(format!(
            "failed to restore backup {} into {}: {e}",
            backup_path.display(),
            primary_path.display()
        )));
    }

    tracing::warn!(
        primary = %primary_path.display(),
        backup = %backup_path.display(),
        quarantined = %quarantined_db.display(),
        "auto-restored sqlite database from backup after corruption detection"
    );
    Ok(())
}

fn ensure_sqlite_file_healthy(primary_path: &Path) -> Result<(), SqlError> {
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    let Some(backup_path) = find_healthy_backup(primary_path) else {
        return Err(SqlError::Custom(format!(
            "database file {} is malformed and no healthy backup was found",
            primary_path.display()
        )));
    };
    restore_from_backup(primary_path, &backup_path)?;
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    Err(SqlError::Custom(format!(
        "database file {} was restored from {}, but quick_check still failed",
        primary_path.display(),
        backup_path.display()
    )))
}

/// Get (or create) a cached pool for the given config.
///
/// Uses a read-first / write-on-miss pattern so concurrent callers sharing
/// the same database URL only take a shared read lock (zero contention on
/// the hot path).  The write lock is only held briefly when creating a new
/// pool — typically once per unique URL during startup.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let cache =
        POOL_CACHE.get_or_init(|| OrderedRwLock::new(LockLevel::DbPoolCache, HashMap::new()));

    // Fast path: shared read lock for existing pool (concurrent readers).
    {
        let guard = cache.read();
        if let Some(pool) = guard.get(&config.database_url) {
            return Ok(pool.clone());
        }
    }

    // Slow path: exclusive write lock to create a new pool (rare).
    let mut guard = cache.write();
    // Double-check after acquiring write lock — another thread may have won the race.
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

        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:?cache=shared".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");

        let config = DbPoolConfig {
            database_url: "sqlite:///relative/path.db".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "relative/path.db");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3?mode=rwc".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3#v1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "postgres://localhost/db".to_string(),
            ..Default::default()
        };
        assert!(config.sqlite_path().is_err());
    }

    #[test]
    fn test_schema_init_in_memory() {
        // Use base schema (no FTS5/triggers) because DbConn is FrankenConnection.

        // Open in-memory connection
        let conn = DbConn::open_memory().expect("failed to open in-memory db");

        // Get base schema SQL (no FTS5 virtual tables or triggers)
        let sql = schema::init_schema_sql_base();
        println!("Schema SQL length: {} bytes", sql.len());

        // Execute it
        conn.execute_raw(&sql).expect("failed to init schema");

        // Verify tables exist by querying them directly (FrankenConnection
        // does not support sqlite_master; use simple SELECT to verify).
        let table_names: Vec<String> = ["projects", "agents", "messages"]
            .iter()
            .filter(|&&t| {
                conn.query_sync(&format!("SELECT 1 FROM {t} LIMIT 0"), &[])
                    .is_ok()
            })
            .map(ToString::to_string)
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
        assert!(
            (10..=50).contains(&min),
            "auto min={min} should be in [10, 50]"
        );
        assert!(
            (50..=200).contains(&max),
            "auto max={max} should be in [50, 200]"
        );
        assert!(max >= min, "max must be >= min");
        // On a 4-core machine: min=16, max=48→50.  On 16-core: min=50, max=192.
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
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

    /// Verify warmup opens the requested number of connections.
    #[test]
    fn pool_warmup_opens_connections() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("warmup_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 10,
            max_connections: 20,
            warmup_connections: 5,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let opened = rt.block_on(pool.warmup(&cx, 5, std::time::Duration::from_secs(10)));
        assert_eq!(opened, 5, "warmup should open exactly 5 connections");

        // Pool stats should reflect the warmed-up connections.
        let stats = pool.pool.stats();
        assert!(
            stats.total_connections >= 5,
            "pool should have at least 5 total connections after warmup, got {}",
            stats.total_connections
        );
    }

    /// Verify warmup with n=0 is a no-op.
    #[test]
    fn pool_warmup_zero_is_noop() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("warmup_zero.db");
        let pool = DbPool::new(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let opened = rt.block_on(pool.warmup(&cx, 0, std::time::Duration::from_secs(1)));
        assert_eq!(opened, 0, "warmup with n=0 should open no connections");
    }

    /// Verify default config includes `warmup_connections`: 0.
    #[test]
    fn default_warmup_is_disabled() {
        let cfg = DbPoolConfig::default();
        assert_eq!(
            cfg.warmup_connections, 0,
            "warmup should be disabled by default"
        );
    }

    /// Verify `build_conn_pragmas` scales `cache_size` with pool size.
    #[test]
    fn build_conn_pragmas_budget_aware_cache() {
        // 100 connections: 512*1024 / 100 = 5242 KB each
        let sql_100 = schema::build_conn_pragmas(100);
        assert!(
            sql_100.contains("cache_size = -5242"),
            "100 conns should get ~5MB each: {sql_100}"
        );

        // 25 connections: 512*1024 / 25 = 20971 KB each
        let sql_25 = schema::build_conn_pragmas(25);
        assert!(
            sql_25.contains("cache_size = -20971"),
            "25 conns should get ~20MB each: {sql_25}"
        );

        // 1 connection: 512*1024 / 1 = 524288 KB → clamped to 65536 (64MB max)
        let sql_1 = schema::build_conn_pragmas(1);
        assert!(
            sql_1.contains("cache_size = -65536"),
            "1 conn should get 64MB (clamped max): {sql_1}"
        );

        // 500 connections: clamped to 2MB min
        let sql_500 = schema::build_conn_pragmas(500);
        assert!(
            sql_500.contains("cache_size = -2048"),
            "500 conns should get 2MB (clamped min): {sql_500}"
        );

        // All should have journal_size_limit
        for sql in [&sql_100, &sql_25, &sql_1, &sql_500] {
            assert!(
                sql.contains("journal_size_limit = 67108864"),
                "all should have 64MB journal_size_limit"
            );
            assert!(
                sql.contains("busy_timeout = 60000"),
                "must have busy_timeout"
            );
            assert!(
                sql.contains("mmap_size = 268435456"),
                "must have 256MB mmap"
            );
        }
    }

    /// Verify `build_conn_pragmas` handles zero pool size gracefully.
    #[test]
    fn build_conn_pragmas_zero_pool_fallback() {
        let sql = schema::build_conn_pragmas(0);
        assert!(
            sql.contains("cache_size = -8192"),
            "0 conns should fallback to 8MB: {sql}"
        );
    }

    /// Verify explicit WAL checkpoint works on a file-backed DB.
    #[test]
    fn wal_checkpoint_succeeds_on_file_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ckpt_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Write some data through the pool to generate WAL entries.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let pool2 = pool.clone();
        rt.block_on(async move {
            let conn = pool2.acquire(&cx).await.unwrap();
            conn.execute_raw("CREATE TABLE IF NOT EXISTS ckpt_test (id INTEGER PRIMARY KEY)")
                .ok();
            conn.execute_raw("INSERT INTO ckpt_test VALUES (1)").ok();
            conn.execute_raw("INSERT INTO ckpt_test VALUES (2)").ok();
        });

        // Checkpoint should succeed without error.
        let frames = pool.wal_checkpoint().expect("checkpoint should succeed");
        // frames can be 0 if autocheckpoint already ran, but it shouldn't error.
        assert!(frames <= 1000, "reasonable frame count: {frames}");
    }

    /// Verify WAL checkpoint on :memory: is a no-op.
    #[test]
    fn wal_checkpoint_noop_for_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let frames = pool
            .wal_checkpoint()
            .expect("memory checkpoint should succeed");
        assert_eq!(frames, 0, "memory DB checkpoint should return 0");
    }

    fn sqlite_marker_value(path: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        let conn = sqlmodel_sqlite::SqliteConnection::open_file(path_str.as_ref()).ok()?;
        conn.execute_raw("CREATE TABLE IF NOT EXISTS marker(value TEXT NOT NULL)")
            .ok()?;
        let rows = conn
            .query_sync("SELECT value FROM marker ORDER BY rowid DESC LIMIT 1", &[])
            .ok()?;
        rows.first()?.get_named::<String>("value").ok()
    }

    #[test]
    fn sqlite_backup_candidates_prioritize_dot_bak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let dot_bak = dir.path().join("storage.sqlite3.bak");
        let backup_series = dir.path().join("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&dot_bak, b"bak").expect("write .bak");
        std::fs::write(&backup_series, b"series").expect("write backup-");

        let candidates = sqlite_backup_candidates(&primary);
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(dot_bak.as_path()),
            ".bak should be first-priority backup candidate"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_restores_from_bak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");
        let primary_str = primary.to_string_lossy();
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(primary_str.as_ref()).expect("open db");
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .expect("create marker table");
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .expect("seed marker");
        drop(conn);
        std::fs::copy(&primary, &backup).expect("copy backup");
        std::fs::write(&primary, b"not-a-sqlite-file").expect("corrupt primary");

        ensure_sqlite_file_healthy(&primary).expect("auto-recovery should succeed");
        assert_eq!(
            sqlite_marker_value(&primary).as_deref(),
            Some("from-backup"),
            "restored DB should preserve backup data"
        );

        let mut corrupt_artifacts = 0usize;
        for entry in std::fs::read_dir(dir.path()).expect("read dir").flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().contains(".corrupt-") {
                corrupt_artifacts += 1;
            }
        }
        assert!(
            corrupt_artifacts >= 1,
            "expected quarantined corrupt artifact(s) after recovery"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_errors_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        std::fs::write(&primary, b"broken").expect("write broken db");

        let err = ensure_sqlite_file_healthy(&primary).expect_err("should fail without backup");
        let message = err.to_string();
        assert!(
            message.contains("no healthy backup"),
            "expected clear no-backup error, got: {message}"
        );
    }
}
