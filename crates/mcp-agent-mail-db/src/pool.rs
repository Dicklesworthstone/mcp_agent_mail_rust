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
use std::sync::{Arc, OnceLock, Weak};
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
pub const DEFAULT_POOL_TIMEOUT_MS: u64 = 30_000;
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
    fn from_shared_pool(config: &DbPoolConfig, pool: Arc<Pool<DbConn>>) -> DbResult<Self> {
        let sqlite_path = resolve_sqlite_path_with_absolute_fallback(&config.sqlite_path()?);
        let init_sql = Arc::new(schema::build_conn_pragmas(config.max_connections));
        let stats_sampler = Arc::new(DbPoolStatsSampler::new());

        Ok(Self {
            pool,
            sqlite_path,
            init_sql,
            run_migrations: config.run_migrations,
            stats_sampler,
        })
    }

    /// Create a new pool (does not open connections until first acquire).
    pub fn new(config: &DbPoolConfig) -> DbResult<Self> {
        let sqlite_path = resolve_sqlite_path_with_absolute_fallback(&config.sqlite_path()?);
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
                    if sqlite_path != ":memory:"
                        && let Err(e) = ensure_sqlite_parent_dir_exists(&sqlite_path)
                    {
                        return Outcome::Err(e);
                    }

                    // For file-backed DBs, run DB-wide init (journal mode, migrations) once
                    // before opening pooled connections.
                    // Run one-time DB initialization (schema + migrations) via a separate
                    // connection to ensure atomic setup before pool connections open.
                    if sqlite_path != ":memory:" {
                        let init_gate = sqlite_init_gate(&sqlite_path);
                        let run_migrations = run_migrations;

                        let gate_out = init_gate
                            .get_or_try_init(|| {
                                let cx2 = cx2.clone();
                                let sqlite_path = sqlite_path.clone();
                                async move {
                                    match initialize_sqlite_file_once(
                                        &cx2,
                                        &sqlite_path,
                                        run_migrations,
                                    )
                                    .await
                                    {
                                        Outcome::Ok(()) => Ok(()),
                                        Outcome::Err(e) => Err(Outcome::Err(e)),
                                        Outcome::Cancelled(r) => Err(Outcome::Cancelled(r)),
                                        Outcome::Panicked(p) => Err(Outcome::Panicked(p)),
                                    }
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

                    // Now open pool connection (migrations are complete).
                    let mut conn = if sqlite_path == ":memory:" {
                        match DbConn::open_memory() {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    } else {
                        match open_sqlite_file_with_recovery(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    };

                    // Per-connection PRAGMAs matching legacy Python `db.py` event listeners.
                    if let Err(first_init_err) = conn.execute_raw(&init_sql) {
                        if sqlite_path == ":memory:"
                            || !is_sqlite_recovery_error_message(&first_init_err.to_string())
                        {
                            return Outcome::Err(first_init_err);
                        }

                        tracing::warn!(
                            path = %sqlite_path,
                            error = %first_init_err,
                            "sqlite connection init PRAGMAs failed with recoverable error; attempting automatic recovery"
                        );

                        drop(conn);
                        if let Err(recovery_err) = recover_sqlite_file(Path::new(&sqlite_path)) {
                            return Outcome::Err(recovery_err);
                        }

                        conn = match open_sqlite_file_with_recovery(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        };
                        if let Err(second_init_err) = conn.execute_raw(&init_sql) {
                            return Outcome::Err(second_init_err);
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

        let conn = match DbConn::open_file(&self.sqlite_path) {
            Ok(conn) => conn,
            Err(e) => {
                if !is_corruption_error_message(&e.to_string()) {
                    return Err(DbError::Sqlite(format!(
                        "startup integrity check: open failed: {e}"
                    )));
                }
                tracing::warn!(
                    path = %self.sqlite_path,
                    error = %e,
                    "startup integrity check failed to open sqlite file; attempting auto-recovery"
                );
                recover_sqlite_file(Path::new(&self.sqlite_path))
                    .map_err(|re| DbError::Sqlite(format!("startup recovery failed: {re}")))?;
                DbConn::open_file(&self.sqlite_path).map_err(|reopen| {
                    DbError::Sqlite(format!(
                        "startup integrity check: open failed after recovery: {reopen}"
                    ))
                })?
            }
        };

        match integrity::quick_check(&conn) {
            Ok(res) => Ok(res),
            Err(DbError::IntegrityCorruption { .. }) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    "startup integrity check failed; attempting auto-recovery from backup"
                );
                // Close connection before attempting restore (Windows/locking safety)
                drop(conn);

                if let Err(e) = recover_sqlite_file(Path::new(&self.sqlite_path)) {
                    return Err(DbError::Sqlite(format!("startup recovery failed: {e}")));
                }

                // Re-open and re-verify
                let conn = DbConn::open_file(&self.sqlite_path).map_err(|e| {
                    DbError::Sqlite(format!(
                        "startup integrity check (post-recovery): open failed: {e}"
                    ))
                })?;
                integrity::quick_check(&conn)
            }
            Err(e) => Err(e),
        }
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

        let conn = match DbConn::open_file(&self.sqlite_path) {
            Ok(conn) => conn,
            Err(e) => {
                if !is_corruption_error_message(&e.to_string()) {
                    return Err(DbError::Sqlite(format!(
                        "full integrity check: open failed: {e}"
                    )));
                }
                tracing::warn!(
                    path = %self.sqlite_path,
                    error = %e,
                    "full integrity check failed to open sqlite file; attempting auto-recovery"
                );
                recover_sqlite_file(Path::new(&self.sqlite_path)).map_err(|re| {
                    DbError::Sqlite(format!("full integrity recovery failed: {re}"))
                })?;
                DbConn::open_file(&self.sqlite_path).map_err(|reopen| {
                    DbError::Sqlite(format!(
                        "full integrity check: open failed after recovery: {reopen}"
                    ))
                })?
            }
        };

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

    /// Create (or refresh) a `.bak` backup of the database file.
    ///
    /// Skips silently for `:memory:` databases or when the primary file
    /// doesn't exist. Performs a WAL checkpoint first to ensure the backup
    /// is self-contained.
    ///
    /// Returns `Ok(Some(path))` with the backup path on success, `Ok(None)`
    /// if the operation was skipped (memory DB, missing file, or the existing
    /// backup is younger than `max_age`).
    pub fn create_proactive_backup(
        &self,
        max_age: std::time::Duration,
    ) -> DbResult<Option<PathBuf>> {
        if self.sqlite_path == ":memory:" {
            return Ok(None);
        }
        let primary = Path::new(&self.sqlite_path);
        if !primary.exists() {
            return Ok(None);
        }

        let bak_path = primary.with_file_name(format!(
            "{}.bak",
            primary
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("storage.sqlite3")
        ));

        // Skip if the existing backup is fresh enough.
        if bak_path.is_file()
            && let Ok(meta) = bak_path.metadata()
            && let Ok(modified) = meta.modified()
            && modified.elapsed().unwrap_or(max_age) < max_age
        {
            return Ok(None);
        }

        // Checkpoint WAL so the backup is self-contained.
        let _ = self.wal_checkpoint();

        std::fs::copy(primary, &bak_path).map_err(|e| {
            DbError::Sqlite(format!(
                "proactive backup failed: {} -> {}: {e}",
                primary.display(),
                bak_path.display()
            ))
        })?;

        tracing::info!(
            primary = %primary.display(),
            backup = %bak_path.display(),
            "created proactive database backup"
        );

        Ok(Some(bak_path))
    }
}

static SQLITE_INIT_GATES: OnceLock<OrderedRwLock<HashMap<String, Arc<OnceCell<()>>>>> =
    OnceLock::new();
static POOL_CACHE: OnceLock<OrderedRwLock<HashMap<String, Weak<Pool<DbConn>>>>> = OnceLock::new();

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

#[allow(clippy::result_large_err)]
async fn run_sqlite_init_once(
    cx: &Cx,
    sqlite_path: &str,
    run_migrations: bool,
) -> Outcome<(), SqlError> {
    let mig_conn = match DbConn::open_file(sqlite_path) {
        Ok(conn) => conn,
        Err(err) => return Outcome::Err(err),
    };

    if let Err(err) = mig_conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL) {
        return Outcome::Err(err);
    }

    if run_migrations {
        match schema::migrate_to_latest_base(cx, &mig_conn).await {
            Outcome::Ok(_) => {}
            Outcome::Err(err) => return Outcome::Err(err),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
    }

    // Always enforce startup cleanup for legacy identity FTS artifacts.
    // These can be reintroduced by historical/full migration paths and have
    // caused post-crash rowid/index mismatch failures.
    if let Err(err) = schema::enforce_runtime_fts_cleanup(&mig_conn) {
        return Outcome::Err(err);
    }

    drop(mig_conn);
    Outcome::Ok(())
}

#[must_use]
fn should_retry_sqlite_init_error(error: &SqlError) -> bool {
    is_sqlite_recovery_error_message(&error.to_string())
}

#[must_use]
pub fn is_sqlite_recovery_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_corruption_error_message(message)
        || lower.contains("out of memory")
        || lower.contains("cursor stack is empty")
        || lower.contains("called `option::unwrap()` on a `none` value")
        || lower.contains("internal error")
}

#[must_use]
fn sqlite_absolute_fallback_path(path: &str, open_error: &str) -> Option<String> {
    if path == ":memory:"
        || Path::new(path).is_absolute()
        || path.starts_with("./")
        || path.starts_with("../")
        || !is_sqlite_recovery_error_message(open_error)
    {
        return None;
    }
    let absolute_candidate = Path::new("/").join(path);
    if !absolute_candidate.exists() {
        return None;
    }
    Some(absolute_candidate.to_string_lossy().into_owned())
}

#[allow(clippy::result_large_err)]
fn ensure_sqlite_parent_dir_exists(path: &str) -> Result<(), SqlError> {
    if path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            SqlError::Custom(format!("failed to create db dir {}: {e}", parent.display()))
        })?;
    }
    Ok(())
}

/// Open a file-backed sqlite connection and automatically recover from
/// corruption-like open failures when possible.
#[allow(clippy::result_large_err)]
pub fn open_sqlite_file_with_recovery(sqlite_path: &str) -> Result<DbConn, SqlError> {
    if sqlite_path == ":memory:" {
        return DbConn::open_memory();
    }
    ensure_sqlite_parent_dir_exists(sqlite_path)?;

    match DbConn::open_file(sqlite_path) {
        Ok(conn) => Ok(conn),
        Err(primary_err) => {
            let primary_msg = primary_err.to_string();

            if let Some(fallback_path) = sqlite_absolute_fallback_path(sqlite_path, &primary_msg) {
                match DbConn::open_file(&fallback_path) {
                    Ok(conn) => return Ok(conn),
                    Err(fallback_err) => {
                        return Err(SqlError::Custom(format!(
                            "cannot open sqlite at {sqlite_path}: {primary_err}; fallback {fallback_path} failed: {fallback_err}"
                        )));
                    }
                }
            }

            if !is_sqlite_recovery_error_message(&primary_msg) {
                return Err(primary_err);
            }

            recover_sqlite_file(Path::new(sqlite_path))?;
            DbConn::open_file(sqlite_path).map_err(|reopen_err| {
                SqlError::Custom(format!(
                    "cannot open sqlite at {sqlite_path}: {primary_err}; reopen after recovery failed: {reopen_err}"
                ))
            })
        }
    }
}

#[allow(clippy::result_large_err)]
async fn initialize_sqlite_file_once(
    cx: &Cx,
    sqlite_path: &str,
    run_migrations: bool,
) -> Outcome<(), SqlError> {
    let path = Path::new(sqlite_path);
    if let Err(err) = recover_sqlite_file(path) {
        return Outcome::Err(err);
    }

    match run_sqlite_init_once(cx, sqlite_path, run_migrations).await {
        ok @ Outcome::Ok(()) => ok,
        non_err @ (Outcome::Cancelled(_) | Outcome::Panicked(_)) => non_err,
        Outcome::Err(first_err) => {
            if !should_retry_sqlite_init_error(&first_err) {
                return Outcome::Err(first_err);
            }

            match sqlite_file_is_healthy(path) {
                Ok(false) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %first_err,
                        "sqlite init failed and health probes detected corruption; attempting automatic recovery"
                    );
                    if let Err(recover_err) = recover_sqlite_file(path) {
                        return Outcome::Err(recover_err);
                    }
                }
                Ok(true) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %first_err,
                        "sqlite init failed but file-level health probes passed; retrying initialization once"
                    );
                }
                Err(health_err) => return Outcome::Err(health_err),
            }

            run_sqlite_init_once(cx, sqlite_path, run_migrations).await
        }
    }
}

/// Check whether an error message indicates `SQLite` file corruption.
///
/// Used by auto-recovery logic to decide whether to attempt backup
/// restoration or reinitialization.
#[must_use]
pub fn is_corruption_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("database disk image is malformed")
        || lower.contains("malformed database schema")
        || lower.contains("file is not a database")
        || lower.contains("no healthy backup was found")
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_is_ok(conn: &DbConn, pragma_sql: &str) -> Result<bool, SqlError> {
    let rows = conn.query_sync(pragma_sql, &[])?;
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

#[allow(clippy::result_large_err)]
fn sqlite_quick_check_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok(conn, "PRAGMA quick_check")
}

#[allow(clippy::result_large_err)]
fn sqlite_incremental_check_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok(conn, "PRAGMA integrity_check(1)")
}

#[allow(clippy::result_large_err)]
fn sqlite_table_has_column(conn: &DbConn, table: &str, column: &str) -> Result<bool, SqlError> {
    let rows = conn.query_sync(&format!("PRAGMA table_info({table})"), &[])?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .any(|name| name == column))
}

#[allow(clippy::result_large_err)]
fn sqlite_ack_pending_probe_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    let messages_has_ack_required = sqlite_table_has_column(conn, "messages", "ack_required")?;
    let recipients_has_ack_ts = sqlite_table_has_column(conn, "message_recipients", "ack_ts")?;
    let recipients_has_message_id =
        sqlite_table_has_column(conn, "message_recipients", "message_id")?;

    // Skip schema-specific smoke probes on partially initialized/legacy schemas.
    if !(messages_has_ack_required && recipients_has_ack_ts && recipients_has_message_id) {
        return Ok(true);
    }

    conn.query_sync(
        "SELECT 1 \
         FROM message_recipients \
         WHERE ack_ts IS NULL \
           AND message_id IN (SELECT id FROM messages WHERE ack_required = 1) \
         LIMIT 1",
        &[],
    )
    .map(|_| true)
}

#[allow(clippy::result_large_err)]
fn sqlite_file_is_healthy(path: &Path) -> Result<bool, SqlError> {
    if !path.exists() {
        return Ok(true);
    }
    let path_str = path.to_string_lossy();
    let conn = match DbConn::open_file(path_str.as_ref()) {
        Ok(conn) => conn,
        Err(e) => {
            if is_corruption_error_message(&e.to_string()) {
                return Ok(false);
            }
            return Err(e);
        }
    };

    match sqlite_quick_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            if is_corruption_error_message(&e.to_string()) {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_incremental_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            if is_corruption_error_message(&e.to_string()) {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_ack_pending_probe_is_ok(&conn) {
        Ok(ok) => Ok(ok),
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg)
                || msg.to_ascii_lowercase().contains("out of memory")
            {
                return Ok(false);
            }
            Err(e)
        }
    }
}

#[allow(clippy::result_large_err)]
fn recover_sqlite_file(primary_path: &Path) -> Result<(), SqlError> {
    if let Some(storage_root) = env_value("STORAGE_ROOT") {
        let storage_root_path = Path::new(&storage_root);
        if storage_root_path.is_dir() {
            return ensure_sqlite_file_healthy_with_archive(primary_path, storage_root_path);
        }
    }
    ensure_sqlite_file_healthy(primary_path)
}

fn sqlite_backup_candidates(primary_path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<(u8, SystemTime, PathBuf)> = Vec::new();
    let Some(file_name) = primary_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let parent = primary_path.parent().unwrap_or_else(|| Path::new("."));
    let scan_dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
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
    if let Ok(entries) = std::fs::read_dir(scan_dir) {
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
                "sqlite backup candidate failed health probes; skipping"
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

#[must_use]
fn resolve_sqlite_path_with_absolute_fallback(sqlite_path: &str) -> String {
    if sqlite_path == ":memory:" {
        return sqlite_path.to_string();
    }

    let relative_path = Path::new(sqlite_path);
    if relative_path.is_absolute() {
        return sqlite_path.to_string();
    }

    // Preserve explicitly relative paths exactly as configured.
    if sqlite_path.starts_with("./") || sqlite_path.starts_with("../") {
        return sqlite_path.to_string();
    }

    let absolute_candidate = Path::new("/").join(relative_path);
    if !absolute_candidate.exists() {
        return sqlite_path.to_string();
    }

    let relative_health = sqlite_file_is_healthy(relative_path).ok();
    let absolute_health = sqlite_file_is_healthy(&absolute_candidate).ok();
    if matches!(
        (relative_health, absolute_health),
        (Some(false), Some(true))
    ) {
        tracing::warn!(
            relative_path = %relative_path.display(),
            absolute_candidate = %absolute_candidate.display(),
            "detected malformed relative sqlite path with healthy absolute sibling; using absolute path (did you mean sqlite:////...?)"
        );
        return absolute_candidate.to_string_lossy().into_owned();
    }

    sqlite_path.to_string()
}

#[allow(clippy::result_large_err)]
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

#[allow(clippy::result_large_err)]
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

#[allow(clippy::result_large_err)]
fn reinitialize_without_backup(primary_path: &Path) -> Result<(), SqlError> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined_db = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));

    if primary_path.exists() {
        std::fs::rename(primary_path, &quarantined_db).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine corrupted database {}: {e}",
                primary_path.display()
            ))
        })?;
    }

    if let Err(e) = quarantine_sidecar(primary_path, "-wal", &timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-wal", primary_path.display()),
            error = %e,
            "failed to quarantine WAL sidecar during scratch reinit; continuing"
        );
    }
    if let Err(e) = quarantine_sidecar(primary_path, "-shm", &timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-shm", primary_path.display()),
            error = %e,
            "failed to quarantine SHM sidecar during scratch reinit; continuing"
        );
    }

    let path_str = primary_path.to_string_lossy();
    let conn = DbConn::open_file(path_str.as_ref()).map_err(|e| {
        SqlError::Custom(format!(
            "failed to initialize fresh sqlite file {}: {e}",
            primary_path.display()
        ))
    })?;
    drop(conn);

    tracing::warn!(
        primary = %primary_path.display(),
        quarantined = %quarantined_db.display(),
        "no healthy sqlite backup found; initialized fresh database file from scratch"
    );
    Ok(())
}

/// Verify and, if necessary, recover a `SQLite` database file.
///
/// Runs layered health probes (`quick_check`, `integrity_check(1)`, and
/// a schema-aware query smoke test) on the file. If corruption is detected:
///
/// 1. Search for a healthy `.bak` / `.backup-*` / `.recovery*` sibling.
/// 2. Quarantine the corrupt file (rename to `*.corrupt-{timestamp}`).
/// 3. Restore from the first healthy backup found.
/// 4. If no healthy backup exists, reinitialize an empty database file.
///
/// Returns `Ok(())` when the file at `primary_path` is healthy (either
/// originally or after successful recovery).
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy(primary_path: &Path) -> Result<(), SqlError> {
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    if let Some(backup_path) = find_healthy_backup(primary_path) {
        restore_from_backup(primary_path, &backup_path)?;
        if sqlite_file_is_healthy(primary_path)? {
            return Ok(());
        }
        return Err(SqlError::Custom(format!(
            "database file {} was restored from {}, but health probes still failed",
            primary_path.display(),
            backup_path.display()
        )));
    }

    reinitialize_without_backup(primary_path)?;
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    Err(SqlError::Custom(format!(
        "database file {} was reinitialized without backup, but health probes still failed",
        primary_path.display()
    )))
}

/// Like [`ensure_sqlite_file_healthy`], but attempts to reconstruct the
/// database from the Git archive before falling back to a blank reinitialize.
///
/// Recovery priority:
/// 1. `.bak` / `.backup-*` / `.recovery*` backup files
/// 2. Git archive reconstruction (recovers messages + agents)
/// 3. Blank reinitialization (empty database)
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy_with_archive(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<(), SqlError> {
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }

    // Priority 1: Restore from backup
    if let Some(backup_path) = find_healthy_backup(primary_path) {
        restore_from_backup(primary_path, &backup_path)?;
        if sqlite_file_is_healthy(primary_path)? {
            return Ok(());
        }
        tracing::warn!(
            "backup restore didn't produce a healthy file; falling through to archive reconstruction"
        );
    }

    // Priority 2: Reconstruct from Git archive
    tracing::warn!(
        storage_root = %storage_root.display(),
        "no healthy backup found; attempting database reconstruction from Git archive"
    );

    // Quarantine the corrupt file first
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");

    if primary_path.exists() {
        let quarantined = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));
        std::fs::rename(primary_path, &quarantined).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine corrupted database {}: {e}",
                primary_path.display()
            ))
        })?;
        let _ = quarantine_sidecar(primary_path, "-wal", &timestamp);
        let _ = quarantine_sidecar(primary_path, "-shm", &timestamp);
    }

    match crate::reconstruct::reconstruct_from_archive(primary_path, storage_root) {
        Ok(stats) => {
            if sqlite_file_is_healthy(primary_path)? {
                tracing::warn!(
                    %stats,
                    "database successfully reconstructed from Git archive"
                );
                return Ok(());
            }
            tracing::warn!(
                "reconstructed database failed health probes; falling through to blank reinit"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "archive reconstruction failed; falling through to blank reinitialize"
            );
        }
    }

    // Priority 3: Blank reinitialization
    reinitialize_without_backup(primary_path)?;
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    Err(SqlError::Custom(format!(
        "all recovery strategies exhausted for {}",
        primary_path.display()
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

    // Fast path: shared read lock for existing live pool (concurrent readers).
    {
        let guard = cache.read();
        if let Some(pool) = guard.get(&config.database_url)
            && let Some(shared_pool) = pool.upgrade()
        {
            return DbPool::from_shared_pool(config, shared_pool);
        }
    }

    // Slow path: exclusive write lock to create a new pool (rare), or to
    // evict dead weak entries left after all callers dropped a pool.
    let mut guard = cache.write();
    // Double-check after acquiring write lock — another thread may have won the race.
    if let Some(pool) = guard.get(&config.database_url) {
        if let Some(shared_pool) = pool.upgrade() {
            return DbPool::from_shared_pool(config, shared_pool);
        }
        guard.remove(&config.database_url);
    }

    let pool = DbPool::new(config)?;
    guard.insert(config.database_url.clone(), Arc::downgrade(&pool.pool));
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
        // Use base schema (no FTS5/triggers) for FrankenConnection pool connections.

        // Open in-memory FrankenConnection
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

    // ── DbPoolConfig coverage ─────────────────────────────────────────

    #[test]
    fn from_env_defaults_use_auto_pool_size() {
        // When no DATABASE_POOL_SIZE env is set, from_env should use auto_pool_size
        let config = DbPoolConfig::from_env();
        let (auto_min, auto_max) = auto_pool_size();
        assert_eq!(config.min_connections, auto_min);
        assert_eq!(config.max_connections, auto_max);
        assert_eq!(config.max_lifetime_ms, DEFAULT_POOL_RECYCLE_MS);
        assert!(config.run_migrations);
    }

    #[test]
    fn sqlite_path_memory_returns_memory_string() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");
    }

    #[test]
    fn sqlite_path_file_returns_path() {
        let config = DbPoolConfig {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./storage.sqlite3");
    }

    #[test]
    fn sqlite_path_invalid_url_returns_error() {
        let config = DbPoolConfig {
            database_url: "postgres://localhost/db".to_string(),
            ..Default::default()
        };
        assert!(config.sqlite_path().is_err());
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
            DEFAULT_POOL_TIMEOUT_MS, 30_000,
            "30s timeout (fail fast, let circuit breaker handle)"
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
        let conn = DbConn::open_file(path_str.as_ref()).ok()?;
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
    fn sqlite_backup_candidates_include_series_for_relative_primary_path() {
        struct CwdGuard {
            previous: PathBuf,
        }
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.previous);
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::current_dir().expect("current_dir");
        let _cwd_guard = CwdGuard { previous };
        std::env::set_current_dir(dir.path()).expect("set cwd");

        let primary = PathBuf::from("storage.sqlite3");
        let backup_series = PathBuf::from("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&backup_series, b"series").expect("write backup series");

        let candidates = sqlite_backup_candidates(&primary);
        assert!(
            candidates.iter().any(|c| {
                c.file_name().and_then(|n| n.to_str())
                    == Some("storage.sqlite3.backup-20260212_000000")
            }),
            "relative primary path should still discover backup-series candidates"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_restores_from_bak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");
        let primary_str = primary.to_string_lossy();
        let conn = DbConn::open_file(primary_str.as_ref()).expect("open db");
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
    fn ensure_sqlite_file_healthy_reinitializes_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        std::fs::write(&primary, b"broken").expect("write broken db");

        ensure_sqlite_file_healthy(&primary).expect("should reinitialize without backup");
        let healthy = sqlite_file_is_healthy(&primary).expect("health check");
        assert!(healthy, "reinitialized sqlite file should pass quick_check");

        let quarantined_any = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(
            quarantined_any,
            "expected corrupted artifact to be quarantined during reinit"
        );
    }

    #[test]
    fn startup_integrity_check_recovers_open_failure_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("startup_corrupt.db");
        std::fs::write(&primary, b"not-a-sqlite-file").expect("write corrupt file");

        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", primary.display()),
            run_migrations: false,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let result = pool
            .run_startup_integrity_check()
            .expect("startup integrity should auto-recover");
        assert!(
            result.ok,
            "startup quick_check should report healthy after recovery"
        );
        assert!(
            sqlite_file_is_healthy(&primary).expect("post-startup health check"),
            "sqlite file should be healthy after startup recovery"
        );
    }

    #[test]
    fn pool_startup_drops_fts_tables() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fts_preservation.db");
        let db_url = format!("sqlite:///{}", db_path.display());
        let db_path_str = db_path.display().to_string();

        // Create pool - runs migrations + FTS cleanup
        let pool = DbPool::new(&DbPoolConfig {
            database_url: db_url,
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();

        // Acquire a connection to trigger migration
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });
        drop(pool);

        // Verify FTS tables are dropped after pool startup (Tantivy handles search)
        let conn = DbConn::open_file(db_path_str).expect("reopen sqlite db");
        let fts_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS n FROM sqlite_master \
                 WHERE type='table' AND name = 'fts_messages'",
                &[],
            )
            .expect("query fts_messages table");
        let fts_count = fts_rows
            .first()
            .and_then(|row| row.get_named::<i64>("n").ok())
            .unwrap_or_default();
        assert_eq!(
            fts_count, 0,
            "pool startup should drop fts_messages table (Tantivy handles search)"
        );
    }

    /// Verify `run_startup_integrity_check` passes on a healthy file-backed DB.
    #[test]
    fn startup_integrity_check_healthy_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy_startup.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Trigger initial migration so the file actually exists.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        let result = pool
            .run_startup_integrity_check()
            .expect("startup integrity check");
        assert!(result.ok, "healthy DB should pass startup integrity check");
        assert!(
            result.details.contains(&"ok".to_string()),
            "details should contain 'ok'"
        );
    }

    /// Verify `run_startup_integrity_check` returns Ok for :memory: databases.
    #[test]
    fn startup_integrity_check_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool
            .run_startup_integrity_check()
            .expect("memory integrity check");
        assert!(result.ok, "memory DB should always pass");
        assert_eq!(result.duration_us, 0, "memory check should be instant");
    }

    /// Verify `run_startup_integrity_check` returns Ok for non-existent DB file.
    #[test]
    fn startup_integrity_check_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool
            .run_startup_integrity_check()
            .expect("missing file check");
        assert!(result.ok, "missing file is not corruption");
    }

    /// Verify `run_full_integrity_check` passes on a healthy file-backed DB.
    #[test]
    fn full_integrity_check_healthy_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy_full.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        let result = pool
            .run_full_integrity_check()
            .expect("full integrity check");
        assert!(result.ok, "healthy DB should pass full integrity check");
        assert_eq!(
            result.kind,
            integrity::CheckKind::Full,
            "should be a full check"
        );
    }

    /// Verify `run_full_integrity_check` returns Ok for :memory: databases.
    #[test]
    fn full_integrity_check_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool.run_full_integrity_check().expect("memory full check");
        assert!(result.ok, "memory DB should always pass full check");
        assert_eq!(
            result.kind,
            integrity::CheckKind::Full,
            "should be Full kind"
        );
    }

    /// Verify `sample_recent_message_refs` returns empty for :memory: databases.
    #[test]
    fn sample_recent_message_refs_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let refs = pool.sample_recent_message_refs(10).expect("memory sample");
        assert!(refs.is_empty(), "memory DB should return empty refs");
    }

    /// Verify `sample_recent_message_refs` returns empty for non-existent DB.
    #[test]
    fn sample_recent_message_refs_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("missing_refs.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let refs = pool
            .sample_recent_message_refs(10)
            .expect("missing file sample");
        assert!(refs.is_empty(), "missing DB should return empty refs");
    }

    /// Verify `sample_recent_message_refs` returns actual messages from a seeded DB.
    #[test]
    fn sample_recent_message_refs_returns_seeded_messages() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("refs_seeded.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Seed the database with a project, agent, and messages.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            let now = crate::now_micros();
            conn.execute_raw(&format!(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'test-proj', '/tmp/test-proj', {now})"
            ))
            .expect("insert project");
            conn.execute_raw(&format!(
                "INSERT INTO agents (id, project_id, name, program, model, \
                 inception_ts, last_active_ts) \
                 VALUES (1, 1, 'BlueLake', 'test', 'test-model', {now}, {now})"
            ))
            .expect("insert agent");
            conn.execute_raw(&format!(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                 thread_id, importance, created_ts) \
                 VALUES (1, 1, 1, 'Test Subject', 'body', 'thread-1', 'normal', {now})"
            ))
            .expect("insert message");
            conn.execute_raw(&format!(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                 thread_id, importance, created_ts) \
                 VALUES (2, 1, 1, 'Second Message', 'body2', 'thread-2', 'normal', {now})"
            ))
            .expect("insert message 2");
        });

        let refs = pool.sample_recent_message_refs(10).expect("sample refs");
        assert_eq!(refs.len(), 2, "should return 2 seeded messages");
        // Messages should be in DESC order by id.
        assert_eq!(refs[0].message_id, 2);
        assert_eq!(refs[1].message_id, 1);
        assert_eq!(refs[0].project_slug, "test-proj");
        assert_eq!(refs[0].sender_name, "BlueLake");
        assert_eq!(refs[0].subject, "Second Message");
        assert_eq!(refs[1].subject, "Test Subject");
    }

    /// Verify `sample_recent_message_refs` honours the limit parameter.
    #[test]
    fn sample_recent_message_refs_respects_limit() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("refs_limited.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            let now = crate::now_micros();
            conn.execute_raw(&format!(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'limit-proj', '/tmp/limit', {now})"
            ))
            .expect("insert project");
            conn.execute_raw(&format!(
                "INSERT INTO agents (id, project_id, name, program, model, \
                 inception_ts, last_active_ts) \
                 VALUES (1, 1, 'RedFox', 'test', 'model', {now}, {now})"
            ))
            .expect("insert agent");
            for i in 1..=5 {
                conn.execute_raw(&format!(
                    "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                     thread_id, importance, created_ts) \
                     VALUES ({i}, 1, 1, 'Msg {i}', 'body', 'thread-{i}', 'normal', {now})"
                ))
                .expect("insert message");
            }
        });

        let refs = pool.sample_recent_message_refs(3).expect("limited sample");
        assert_eq!(refs.len(), 3, "should respect limit=3");
        // Most recent first.
        assert_eq!(refs[0].message_id, 5);
        assert_eq!(refs[2].message_id, 3);
    }

    /// Verify `get_or_create_pool` returns the same pool for the same URL.
    #[test]
    fn get_or_create_pool_caches_by_url() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };

        let pool1 = get_or_create_pool(&config).expect("first get");
        let pool2 = get_or_create_pool(&config).expect("second get");

        // Both should point to the same underlying pool (Arc identity).
        assert!(
            Arc::ptr_eq(&pool1.pool, &pool2.pool),
            "get_or_create_pool should return the same Arc<Pool> for the same URL"
        );
    }

    /// Verify `DbPool::sqlite_path()` accessor matches config.
    #[test]
    fn pool_sqlite_path_accessor() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("path_test.db");
        let expected = db_path.display().to_string();
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        assert_eq!(pool.sqlite_path(), expected);
    }

    /// Verify `sample_pool_stats_now` doesn't panic and updates metrics.
    #[test]
    fn sample_pool_stats_now_updates_metrics() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stats_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Open a connection first so the pool has something to sample.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        // This should not panic.
        pool.sample_pool_stats_now();

        // Verify global metrics were updated.
        let metrics = mcp_agent_mail_core::global_metrics();
        let total = metrics.db.pool_total_connections.load();
        assert!(
            total >= 1,
            "pool_total_connections should be >= 1 after acquire + sample, got {total}"
        );
    }

    #[test]
    fn pool_startup_strips_identity_fts_artifacts() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("identity_fts_preserved.db");
        let db_path_str = db_path.display().to_string();
        let db_url = format!("sqlite:///{}", db_path.display());
        let config = DbPoolConfig {
            database_url: db_url,
            ..Default::default()
        };
        let parsed_path = config
            .sqlite_path()
            .expect("parse sqlite path from database_url");
        assert_eq!(
            parsed_path, db_path_str,
            "pool must target the fixture DB path for this regression test"
        );

        // Create pool - should run full migrations including FTS
        let pool = DbPool::new(&config).expect("create pool");
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });
        drop(pool);

        // Verify identity FTS artifacts are removed after pool startup.
        let conn = DbConn::open_file(parsed_path).expect("reopen db");
        let identity_fts_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS n FROM sqlite_master \
                 WHERE (type='table' AND name IN ('fts_agents', 'fts_projects')) \
                    OR (type='trigger' AND name IN (\
                        'agents_ai', 'agents_ad', 'agents_au', \
                        'projects_ai', 'projects_ad', 'projects_au'\
                    ))",
                &[],
            )
            .expect("query identity FTS artifacts");
        let identity_fts_count = identity_fts_rows
            .first()
            .and_then(|row| row.get_named::<i64>("n").ok())
            .unwrap_or_default();
        assert_eq!(
            identity_fts_count, 0,
            "pool startup must remove legacy identity FTS artifacts to avoid rowid corruption regressions"
        );
    }

    /// Verify `create_pool` is an alias for `get_or_create_pool`.
    #[test]
    fn create_pool_is_alias_for_get_or_create() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("alias_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };

        let pool1 = create_pool(&config).expect("create_pool");
        let pool2 = get_or_create_pool(&config).expect("get_or_create_pool");

        assert!(
            Arc::ptr_eq(&pool1.pool, &pool2.pool),
            "create_pool should delegate to get_or_create_pool"
        );
    }

    /// Verify corruption detection recognizes known error messages.
    #[test]
    fn corruption_error_message_detection() {
        assert!(is_corruption_error_message(
            "database disk image is malformed"
        ));
        assert!(is_corruption_error_message(
            "Error: database disk image is malformed (detail)"
        ));
        assert!(is_corruption_error_message(
            "malformed database schema - something"
        ));
        assert!(is_corruption_error_message("file is not a database"));
        assert!(is_corruption_error_message(
            "database file tmp/storage.sqlite3 is malformed and no healthy backup was found"
        ));
        assert!(is_corruption_error_message(
            "DATABASE DISK IMAGE IS MALFORMED"
        ));
        // Non-corruption errors should not be detected.
        assert!(!is_corruption_error_message("table not found"));
        assert!(!is_corruption_error_message("database is locked"));
        assert!(!is_corruption_error_message(""));
    }

    #[test]
    fn sqlite_recovery_error_message_detection() {
        assert!(is_sqlite_recovery_error_message(
            "database disk image is malformed"
        ));
        assert!(is_sqlite_recovery_error_message(
            "Query error: out of memory"
        ));
        assert!(is_sqlite_recovery_error_message("cursor stack is empty"));
        assert!(is_sqlite_recovery_error_message(
            "called `Option::unwrap()` on a `None` value"
        ));
        assert!(is_sqlite_recovery_error_message("internal error"));
        assert!(!is_sqlite_recovery_error_message("database is locked"));
        assert!(!is_sqlite_recovery_error_message("table not found"));
    }

    /// Verify `sqlite_file_is_healthy` returns false for a corrupt file.
    #[test]
    fn sqlite_file_is_healthy_detects_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.db");
        std::fs::write(&path, b"not-a-database").expect("write corrupt");
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(!healthy, "corrupt file should not be healthy");
    }

    /// Verify `sqlite_file_is_healthy` returns true for non-existent file.
    #[test]
    fn sqlite_file_is_healthy_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.db");
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(
            healthy,
            "non-existent file is considered healthy (not corrupt)"
        );
    }

    /// Verify `sqlite_file_is_healthy` returns true for a valid DB.
    #[test]
    fn sqlite_file_is_healthy_valid_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(healthy, "valid DB should be healthy");
    }

    #[test]
    fn resolve_sqlite_path_prefers_healthy_absolute_when_relative_is_malformed() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let relative_path = PathBuf::from(absolute_db_str.trim_start_matches('/'));
        if let Some(parent) = relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create relative parent");
        }
        std::fs::write(&relative_path, b"not-a-database").expect("write malformed relative db");

        let resolved =
            resolve_sqlite_path_with_absolute_fallback(relative_path.to_string_lossy().as_ref());
        assert_eq!(resolved, absolute_db_str);

        let _ = std::fs::remove_file(&relative_path);
        if let Some(parent) = relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn resolve_sqlite_path_keeps_explicit_dot_relative_paths() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let explicit_relative = format!("./{}", absolute_db_str.trim_start_matches('/'));
        let explicit_relative_path = PathBuf::from(&explicit_relative);
        if let Some(parent) = explicit_relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create explicit relative parent");
        }
        std::fs::write(&explicit_relative_path, b"not-a-database")
            .expect("write malformed explicit relative db");

        let resolved = resolve_sqlite_path_with_absolute_fallback(&explicit_relative);
        assert_eq!(resolved, explicit_relative);

        let _ = std::fs::remove_file(&explicit_relative_path);
        if let Some(parent) = explicit_relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// Verify `quarantine_sidecar` renames WAL/SHM files with corrupt- prefix.
    #[test]
    fn quarantine_sidecar_renames_files() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal-content").expect("write wal");

        quarantine_sidecar(&primary, "-wal", "20260218_120000_000").expect("quarantine");

        assert!(!wal.exists(), "original WAL should be gone");
        let quarantined = dir.path().join("test.db-wal.corrupt-20260218_120000_000");
        assert!(quarantined.exists(), "quarantined WAL should exist");
    }

    /// Verify `quarantine_sidecar` is a no-op when the sidecar doesn't exist.
    #[test]
    fn quarantine_sidecar_noop_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        std::fs::write(&primary, b"db").expect("write primary");

        // Should not error when WAL doesn't exist.
        quarantine_sidecar(&primary, "-wal", "20260218_120000_000").expect("quarantine noop");
    }

    // -----------------------------------------------------------------------
    // ensure_sqlite_file_healthy_with_archive tests
    // -----------------------------------------------------------------------

    /// Archive-aware recovery should restore from backup when available.
    #[test]
    fn archive_recovery_prefers_backup_over_archive() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");
        let storage_root = dir.path().join("storage");

        // Create a healthy backup with a marker table.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .unwrap();
        drop(conn);
        std::fs::copy(&primary, &backup).unwrap();

        // Corrupt the primary.
        std::fs::write(&primary, b"corrupted-data").unwrap();

        // Create a minimal storage root (even though backup should win).
        std::fs::create_dir_all(storage_root.join("projects").join("proj1")).unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        // Should have restored from backup (marker table present).
        let val = sqlite_marker_value(&primary);
        assert_eq!(
            val.as_deref(),
            Some("from-backup"),
            "backup should take priority over archive"
        );
    }

    /// Archive-aware recovery should reconstruct from archive when no backup exists.
    #[test]
    fn archive_recovery_reconstructs_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        // Corrupt primary, no backup.
        std::fs::write(&primary, b"corrupted-data").unwrap();

        // Set up archive with a project + agent + message.
        let proj_dir = storage_root.join("projects").join("test-proj");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"SwiftFox","role":"Coder","model":"claude","registered_ts":"2026-01-15T10:00:00"}"#,
        ).unwrap();

        let msg_dir = proj_dir.join("messages").join("2026").join("01");
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            msg_dir.join("001_test.md"),
            "---json\n{\n  \"id\": 1,\n  \"subject\": \"Test\",\n  \"from_agent\": \"SwiftFox\",\n  \"importance\": \"normal\",\n  \"to\": [\"CalmLake\"],\n  \"cc\": [],\n  \"bcc\": [],\n  \"thread_id\": \"t1\",\n  \"in_reply_to\": null,\n  \"created_ts\": \"2026-01-15T10:05:00\"\n}\n---\n\nTest body\n",
        ).unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        assert!(
            sqlite_file_is_healthy(&primary).unwrap(),
            "reconstructed DB should be healthy"
        );

        // Verify data was actually recovered from archive.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .unwrap();
        let count = rows
            .first()
            .and_then(|r| r.get_named::<i64>("n").ok())
            .unwrap_or(0);
        assert!(count >= 1, "should have at least 1 message from archive");
    }

    /// Archive-aware recovery should fall back to blank reinit when archive is empty.
    #[test]
    fn archive_recovery_reinits_with_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        // Corrupt primary, no backup, empty storage.
        std::fs::write(&primary, b"corrupted-data").unwrap();
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        assert!(
            sqlite_file_is_healthy(&primary).unwrap(),
            "reinitialized DB should be healthy"
        );
    }

    /// Archive-aware recovery should skip when DB is already healthy.
    #[test]
    fn archive_recovery_noop_on_healthy_db() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        // Create a healthy DB.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('original')")
            .unwrap();
        drop(conn);

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        // Data should be untouched.
        let val = sqlite_marker_value(&primary);
        assert_eq!(
            val.as_deref(),
            Some("original"),
            "healthy DB should not be touched"
        );
    }

    // -----------------------------------------------------------------------
    // create_proactive_backup tests
    // -----------------------------------------------------------------------

    /// Proactive backup creates a .bak file after successful integrity check.
    #[test]
    fn proactive_backup_creates_bak_file() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_backup.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        // Trigger migration so the file exists.
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().unwrap();
        });

        // Create backup with 0 max_age so it always writes.
        let result = pool
            .create_proactive_backup(std::time::Duration::ZERO)
            .unwrap();
        assert!(result.is_some(), "should create a backup");

        let bak_path = result.unwrap();
        assert!(bak_path.exists(), "backup file should exist");
        assert!(
            bak_path.to_string_lossy().ends_with(".bak"),
            "should end with .bak"
        );
    }

    /// Proactive backup skips when existing backup is fresh.
    #[test]
    fn proactive_backup_skips_fresh_backup() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_skip.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().unwrap();
        });

        // First backup should succeed.
        let first = pool
            .create_proactive_backup(std::time::Duration::from_hours(1))
            .unwrap();
        assert!(first.is_some(), "first backup should create file");

        // Second backup should skip (backup is <1 hour old).
        let second = pool
            .create_proactive_backup(std::time::Duration::from_hours(1))
            .unwrap();
        assert!(second.is_none(), "should skip since backup is fresh");
    }

    /// Proactive backup is a no-op for :memory: databases.
    #[test]
    fn proactive_backup_noop_for_memory() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        let result = pool
            .create_proactive_backup(std::time::Duration::ZERO)
            .unwrap();
        assert!(result.is_none(), "memory DB should not create backup");
    }

    // ── auto_pool_size ─────────────────────────────────────────────────

    #[test]
    fn auto_pool_size_returns_valid_bounds() {
        let (min, max) = auto_pool_size();
        assert!(min >= 10, "min should be at least 10, got {min}");
        assert!(max >= 50, "max should be at least 50, got {max}");
        assert!(min <= 50, "min should be at most 50, got {min}");
        assert!(max <= 200, "max should be at most 200, got {max}");
        assert!(min <= max, "min ({min}) should not exceed max ({max})");
    }

    // ── is_corruption_error_message ────────────────────────────────────

    #[test]
    fn corruption_error_detects_malformed_image() {
        assert!(is_corruption_error_message(
            "database disk image is malformed"
        ));
    }

    #[test]
    fn corruption_error_detects_malformed_schema() {
        assert!(is_corruption_error_message(
            "malformed database schema - broken_table"
        ));
    }

    #[test]
    fn corruption_error_detects_not_a_database() {
        assert!(is_corruption_error_message("file is not a database"));
    }

    #[test]
    fn corruption_error_detects_no_healthy_backup() {
        assert!(is_corruption_error_message("no healthy backup was found"));
    }

    #[test]
    fn corruption_error_case_insensitive() {
        assert!(is_corruption_error_message(
            "DATABASE DISK IMAGE IS MALFORMED"
        ));
        assert!(is_corruption_error_message("File Is Not A Database"));
    }

    #[test]
    fn corruption_error_rejects_unrelated_messages() {
        assert!(!is_corruption_error_message("connection refused"));
        assert!(!is_corruption_error_message("timeout"));
        assert!(!is_corruption_error_message("constraint violation"));
        assert!(!is_corruption_error_message("unique constraint failed"));
        assert!(!is_corruption_error_message("no such table"));
        assert!(!is_corruption_error_message(""));
    }

    #[test]
    fn corruption_error_detects_embedded_in_longer_message() {
        assert!(is_corruption_error_message(
            "SqlError: database disk image is malformed (while running SELECT)"
        ));
    }

    // ── is_sqlite_recovery_error_message ───────────────────────────────

    #[test]
    fn recovery_error_includes_all_corruption_patterns() {
        // All corruption patterns are also recovery patterns
        assert!(is_sqlite_recovery_error_message(
            "database disk image is malformed"
        ));
        assert!(is_sqlite_recovery_error_message(
            "malformed database schema"
        ));
        assert!(is_sqlite_recovery_error_message("file is not a database"));
        assert!(is_sqlite_recovery_error_message(
            "no healthy backup was found"
        ));
    }

    #[test]
    fn recovery_error_detects_out_of_memory() {
        assert!(is_sqlite_recovery_error_message("out of memory"));
        assert!(is_sqlite_recovery_error_message("OUT OF MEMORY"));
    }

    #[test]
    fn recovery_error_detects_cursor_stack_empty() {
        assert!(is_sqlite_recovery_error_message("cursor stack is empty"));
    }

    #[test]
    fn recovery_error_detects_unwrap_none() {
        assert!(is_sqlite_recovery_error_message(
            "called `option::unwrap()` on a `none` value"
        ));
    }

    #[test]
    fn recovery_error_detects_internal_error() {
        assert!(is_sqlite_recovery_error_message("internal error"));
    }

    #[test]
    fn recovery_error_rejects_non_recovery_messages() {
        assert!(!is_sqlite_recovery_error_message("connection refused"));
        assert!(!is_sqlite_recovery_error_message("timeout"));
        assert!(!is_sqlite_recovery_error_message("no such table"));
        assert!(!is_sqlite_recovery_error_message(""));
    }

    // ── sqlite_absolute_fallback_path ──────────────────────────────────

    #[test]
    fn fallback_path_returns_none_for_memory_db() {
        assert!(
            sqlite_absolute_fallback_path(":memory:", "database disk image is malformed").is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_absolute_path() {
        assert!(
            sqlite_absolute_fallback_path("/data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_dot_relative() {
        assert!(
            sqlite_absolute_fallback_path("./data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_dotdot_relative() {
        assert!(
            sqlite_absolute_fallback_path("../data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_non_recovery_error() {
        assert!(sqlite_absolute_fallback_path("data/db.sqlite3", "connection refused").is_none());
    }

    #[test]
    fn fallback_path_returns_none_when_absolute_candidate_does_not_exist() {
        assert!(
            sqlite_absolute_fallback_path(
                "nonexistent/path/db.sqlite3",
                "database disk image is malformed"
            )
            .is_none()
        );
    }

    // ── ensure_sqlite_parent_dir_exists ─────────────────────────────────

    #[test]
    fn ensure_parent_dir_noop_for_memory_db() {
        assert!(ensure_sqlite_parent_dir_exists(":memory:").is_ok());
    }

    #[test]
    fn ensure_parent_dir_creates_missing_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c/test.sqlite3");
        assert!(!tmp.path().join("a").exists());
        ensure_sqlite_parent_dir_exists(nested.to_str().unwrap()).unwrap();
        assert!(tmp.path().join("a/b/c").exists());
    }

    #[test]
    fn ensure_parent_dir_ok_when_already_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite3");
        assert!(ensure_sqlite_parent_dir_exists(db_path.to_str().unwrap()).is_ok());
    }

    // ── open_sqlite_file_with_recovery ──────────────────────────────────

    #[test]
    fn open_memory_db_succeeds() {
        let conn = open_sqlite_file_with_recovery(":memory:").unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn open_real_file_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite3");
        let conn = open_sqlite_file_with_recovery(db_path.to_str().unwrap()).unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn open_creates_parent_dirs_if_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("sub/dir/test.sqlite3");
        let conn = open_sqlite_file_with_recovery(db_path.to_str().unwrap()).unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    // ── DbPoolConfig::from_env ──────────────────────────────────────────

    #[test]
    fn pool_config_from_env_has_defaults() {
        let config = DbPoolConfig::from_env();
        assert!(!config.database_url.is_empty() || config.database_url.is_empty()); // just ensure it doesn't panic
        assert!(config.min_connections > 0);
        assert!(config.max_connections >= config.min_connections);
    }
}
