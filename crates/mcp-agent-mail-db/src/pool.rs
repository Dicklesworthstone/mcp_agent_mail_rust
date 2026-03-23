//! Connection pool configuration and initialization
//!
//! Uses `sqlmodel_pool` for efficient connection management.

use crate::DbConn;
use crate::error::{DbError, DbResult, is_lock_error};
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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone)]
struct SampledMessage {
    id: i64,
    project_id: i64,
    sender_id: i64,
    subject: String,
    created_ts_iso: String,
}

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

        let total = u64::try_from(stats.total_connections).unwrap_or(0);
        let idle = u64::try_from(stats.idle_connections).unwrap_or(0);
        let active = u64::try_from(stats.active_connections).unwrap_or(0);
        let pending = u64::try_from(stats.pending_requests).unwrap_or(0);

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

    #[must_use]
    pub fn sqlite_identity_key(&self) -> String {
        if self.sqlite_path == ":memory:" {
            format!(":memory:@{:p}", Arc::as_ptr(&self.pool))
        } else {
            self.sqlite_path.clone()
        }
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
                    if let Err(first_init_err) = execute_sql_with_lock_retry(
                        &conn,
                        &sqlite_path,
                        &init_sql,
                        "pool connection init pragmas",
                    ) {
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

                        crate::close_db_conn(conn, "sqlite connection init before recovery");
                        if let Err(recovery_err) = recover_sqlite_file(Path::new(&sqlite_path)) {
                            return Outcome::Err(recovery_err);
                        }

                        conn = match open_sqlite_file_with_recovery(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        };
                        if let Err(second_init_err) = execute_sql_with_lock_retry(
                            &conn,
                            &sqlite_path,
                            &init_sql,
                            "pool connection init pragmas after recovery",
                        ) {
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

        // Check if the file exists first. If missing, it requires recovery (e.g. from archive or backup).
        if !Path::new(&self.sqlite_path).exists() {
            return Err(DbError::IntegrityCorruption {
                message: "Database file is missing".to_string(),
                details: vec!["File not found on disk".to_string()],
            });
        }

        let conn = crate::guard_db_conn(
            match open_sqlite_file_with_lock_retry(&self.sqlite_path) {
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
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|reopen| {
                        DbError::Sqlite(format!(
                            "startup integrity check: open failed after recovery: {reopen}"
                        ))
                    })?
                }
            },
            "startup integrity check connection",
        );

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
                let conn = crate::guard_db_conn(
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|e| {
                        DbError::Sqlite(format!(
                            "startup integrity check (post-recovery): open failed: {e}"
                        ))
                    })?,
                    "startup integrity check post-recovery connection",
                );
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
            return Err(DbError::IntegrityCorruption {
                message: "Database file is missing".to_string(),
                details: vec!["File not found on disk".to_string()],
            });
        }

        let conn = crate::guard_db_conn(
            match open_sqlite_file_with_lock_retry(&self.sqlite_path) {
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
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|reopen| {
                        DbError::Sqlite(format!(
                            "full integrity check: open failed after recovery: {reopen}"
                        ))
                    })?
                }
            },
            "full integrity check connection",
        );

        integrity::full_check(&conn)
    }

    /// Sample the N most recent messages from the DB for consistency checking.
    ///
    /// Returns lightweight refs that the storage layer can use to verify
    /// archive file presence. Opens a dedicated connection (outside the pool)
    /// so this works even if the pool isn't fully started yet.
    #[allow(clippy::too_many_lines)]
    pub fn sample_recent_message_refs(&self, limit: i64) -> DbResult<Vec<ConsistencyMessageRef>> {
        if self.sqlite_path == ":memory:" {
            return Ok(Vec::new());
        }
        if !Path::new(&self.sqlite_path).exists() {
            return Ok(Vec::new());
        }

        // Keep consistency sampling on FrankenSQLite and avoid JOIN-heavy scans:
        // 1) fetch recent envelopes
        // 2) resolve slugs/names via batched point lookups
        let conn = crate::guard_db_conn(
            open_sqlite_file_with_lock_retry(&self.sqlite_path)
                .map_err(|e| DbError::Sqlite(format!("consistency probe: open failed: {e}")))?,
            "consistency probe connection",
        );
        // This two-phase strategy is materially faster than a three-way JOIN on
        // large mailboxes and reduces startup probe lock contention.
        let message_rows = conn
            .query_sync(
                "SELECT id, project_id, sender_id, subject, created_ts \
                 FROM messages \
                 ORDER BY id DESC \
                 LIMIT ?",
                &[sqlmodel_core::Value::BigInt(limit)],
            )
            .map_err(|e| DbError::Sqlite(format!("consistency probe query: {e}")))?;

        if message_rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut sampled: Vec<SampledMessage> = Vec::with_capacity(message_rows.len());
        let mut project_ids: Vec<i64> = Vec::new();
        let mut sender_ids: Vec<i64> = Vec::new();
        let mut seen_projects: HashSet<i64> = HashSet::new();
        let mut seen_senders: HashSet<i64> = HashSet::new();

        for row in &message_rows {
            let id = match row.get_by_name("id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let project_id = match row.get_by_name("project_id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let sender_id = match row.get_by_name("sender_id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let subject = match row.get_by_name("subject") {
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let created_ts_iso = match row.get_by_name("created_ts") {
                Some(sqlmodel_core::Value::BigInt(us)) => crate::micros_to_iso(*us),
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };

            if seen_projects.insert(project_id) {
                project_ids.push(project_id);
            }
            if seen_senders.insert(sender_id) {
                sender_ids.push(sender_id);
            }
            sampled.push(SampledMessage {
                id,
                project_id,
                sender_id,
                subject,
                created_ts_iso,
            });
        }

        if sampled.is_empty() {
            return Ok(Vec::new());
        }

        let mut project_slugs_by_id: HashMap<i64, String> = HashMap::new();
        if !project_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", project_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT id, slug FROM projects WHERE id IN ({placeholders})");
            let params = project_ids
                .iter()
                .copied()
                .map(sqlmodel_core::Value::BigInt)
                .collect::<Vec<_>>();
            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(format!("consistency probe project lookup: {e}")))?;
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
                project_slugs_by_id.insert(id, slug);
            }
        }

        let mut sender_names_by_id: HashMap<i64, String> = HashMap::new();
        if !sender_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", sender_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT id, name FROM agents WHERE id IN ({placeholders})");
            let params = sender_ids
                .iter()
                .copied()
                .map(sqlmodel_core::Value::BigInt)
                .collect::<Vec<_>>();
            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(format!("consistency probe agent lookup: {e}")))?;
            for row in &rows {
                let id = match row.get_by_name("id") {
                    Some(sqlmodel_core::Value::BigInt(n)) => *n,
                    Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let name = match row.get_by_name("name") {
                    Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                    _ => continue,
                };
                sender_names_by_id.insert(id, name);
            }
        }

        let mut refs = Vec::with_capacity(sampled.len());
        for message in sampled {
            let Some(project_slug) = project_slugs_by_id.get(&message.project_id) else {
                continue;
            };
            let Some(sender_name) = sender_names_by_id.get(&message.sender_id) else {
                continue;
            };
            refs.push(ConsistencyMessageRef {
                project_slug: project_slug.clone(),
                message_id: message.id,
                sender_name: sender_name.clone(),
                subject: message.subject,
                created_ts_iso: message.created_ts_iso,
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
        let conn = crate::guard_db_conn(
            open_sqlite_file_with_lock_retry(&self.sqlite_path)
                .map_err(|e| DbError::Sqlite(format!("checkpoint: open failed: {e}")))?,
            "wal checkpoint connection",
        );

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
        if let Err(e) = self.wal_checkpoint() {
            return Err(DbError::Sqlite(format!(
                "proactive backup aborted: WAL checkpoint failed for {}: {e}",
                primary.display()
            )));
        }

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

    /// Attempt one-shot recovery from `SQLite` corruption detected at runtime.
    ///
    /// This should be called when a query returns a corruption error
    /// (e.g. "database disk image is malformed"). The method:
    ///
    /// 1. Logs the corruption event
    /// 2. Attempts recovery via backup restore or archive reconstruction
    /// 3. Returns `Ok(true)` if recovery succeeded, `Ok(false)` if the DB
    ///    is in-memory (no recovery possible), or `Err` if recovery failed.
    ///
    /// After a successful recovery, callers should retry their operation
    /// by re-acquiring a connection from the pool.
    ///
    /// Uses a global flag to prevent concurrent recovery attempts.
    pub fn try_recover_from_corruption(&self, trigger_error: &str) -> DbResult<bool> {
        // Use a global flag to serialize recovery attempts. Only one thread
        // should attempt recovery at a time.
        static RECOVERY_IN_PROGRESS: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        struct ResetOnDrop;
        impl Drop for ResetOnDrop {
            fn drop(&mut self) {
                RECOVERY_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        if self.sqlite_path == ":memory:" {
            return Ok(false);
        }

        if RECOVERY_IN_PROGRESS
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            tracing::warn!(
                "runtime corruption recovery already in progress; skipping duplicate attempt"
            );
            return Ok(false);
        }

        let _guard = ResetOnDrop;

        tracing::error!(
            path = %self.sqlite_path,
            trigger = %trigger_error,
            "runtime corruption detected; attempting automatic recovery"
        );

        let primary_path = Path::new(&self.sqlite_path);
        match sqlite_file_is_healthy(primary_path) {
            Ok(true) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    trigger = %trigger_error,
                    "runtime corruption trigger received, but health probes already pass; skipping recovery"
                );
                return Ok(false);
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    trigger = %trigger_error,
                    error = %e,
                    "failed to run pre-recovery health probes; proceeding with recovery attempt"
                );
            }
        }

        // Record the corruption event in metrics only for confirmed/suspected unhealthy state.
        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.db.integrity_failures_total.inc();

        match recover_sqlite_file(primary_path) {
            Ok(()) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    "runtime corruption recovery succeeded — clearing idle connections and returning to service"
                );
                // Idle connections hold FDs to the old (corrupted/unlinked) inode.
                // test_on_checkout(true) ensures stale connections fail health checks
                // and get evicted on next acquire, so explicit draining is not required.
                Ok(true)
            }
            Err(e) => {
                tracing::error!(
                    path = %self.sqlite_path,
                    error = %e,
                    "runtime corruption recovery FAILED — manual intervention required (try: am doctor repair)"
                );
                Err(DbError::IntegrityCorruption {
                    message: format!(
                        "Database corruption detected and automatic recovery failed: {e}. \
                         Run 'am doctor repair' or 'am doctor reconstruct' to manually recover."
                    ),
                    details: vec![trigger_error.to_string()],
                })
            }
        }
    }
}

static SQLITE_INIT_GATES: OnceLock<OrderedRwLock<HashMap<String, Arc<OnceCell<()>>>>> =
    OnceLock::new();
static POOL_CACHE: OnceLock<OrderedRwLock<HashMap<String, Weak<Pool<DbConn>>>>> = OnceLock::new();
static SQLITE_IDENTITY_PATH_CACHE: OnceLock<Mutex<HashMap<String, SqliteIdentityPathCacheEntry>>> =
    OnceLock::new();

#[derive(Clone, Debug)]
struct SqliteIdentityPathCacheEntry {
    normalized: String,
    validated_at: Instant,
}

const SQLITE_IDENTITY_PATH_CACHE_MAX_ENTRIES: usize = 256;
#[cfg(test)]
const SQLITE_IDENTITY_PATH_CACHE_FRESHNESS: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const SQLITE_IDENTITY_PATH_CACHE_FRESHNESS: Duration = Duration::from_secs(2);

fn sqlite_identity_path_cache() -> &'static Mutex<HashMap<String, SqliteIdentityPathCacheEntry>> {
    SQLITE_IDENTITY_PATH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sqlite_identity_path_cache_get(path: &str) -> Option<String> {
    let mut cache = sqlite_identity_path_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache.get(path)?;
    if entry.validated_at.elapsed() <= SQLITE_IDENTITY_PATH_CACHE_FRESHNESS {
        return Some(entry.normalized.clone());
    }
    cache.remove(path);
    None
}

fn sqlite_identity_path_cache_insert(path: &str, normalized: &str) {
    let mut cache = sqlite_identity_path_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !cache.contains_key(path)
        && cache.len() >= SQLITE_IDENTITY_PATH_CACHE_MAX_ENTRIES
        && let Some(victim) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.validated_at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&victim);
    }
    cache.insert(
        path.to_string(),
        SqliteIdentityPathCacheEntry {
            normalized: normalized.to_string(),
            validated_at: Instant::now(),
        },
    );
}

#[must_use]
fn normalize_sqlite_identity_path(path: &str) -> String {
    if path == ":memory:" {
        return path.to_string();
    }
    if let Some(cached) = sqlite_identity_path_cache_get(path) {
        return cached;
    }
    let as_path = Path::new(path);
    let normalized = std::fs::canonicalize(as_path).map_or_else(
        |_| {
            if as_path.is_absolute() {
                as_path.to_string_lossy().into_owned()
            } else if let Ok(cwd) = std::env::current_dir() {
                cwd.join(as_path).to_string_lossy().into_owned()
            } else {
                path.to_string()
            }
        },
        |canonical| canonical.to_string_lossy().into_owned(),
    );
    sqlite_identity_path_cache_insert(path, &normalized);
    normalized
}

#[must_use]
fn pool_cache_key(config: &DbPoolConfig) -> String {
    let identity = config.sqlite_path().map_or_else(
        |_| config.database_url.clone(),
        |parsed| {
            let resolved = resolve_sqlite_path_with_absolute_fallback(&parsed);
            normalize_sqlite_identity_path(&resolved)
        },
    );
    format!(
        "{identity}|min={}|max={}|acquire_ms={}|lifetime_ms={}",
        config.min_connections,
        config.max_connections,
        config.acquire_timeout_ms,
        config.max_lifetime_ms
    )
}

fn sqlite_init_gate(sqlite_path: &str) -> Arc<OnceCell<()>> {
    let gate_key = normalize_sqlite_identity_path(sqlite_path);
    let gates = SQLITE_INIT_GATES
        .get_or_init(|| OrderedRwLock::new(LockLevel::DbSqliteInitGates, HashMap::new()));

    // Fast path: read lock for existing gate (concurrent readers).
    {
        let guard = gates.read();
        if let Some(gate) = guard.get(&gate_key) {
            return Arc::clone(gate);
        }
    }

    // Slow path: write lock to create a new gate (rare, once per SQLite file).
    let mut guard = gates.write();
    // Double-check after acquiring write lock.
    if let Some(gate) = guard.get(&gate_key) {
        return Arc::clone(gate);
    }
    let gate = Arc::new(OnceCell::new());
    guard.insert(gate_key, Arc::clone(&gate));
    gate
}

#[allow(clippy::result_large_err)]
async fn run_sqlite_init_once(
    cx: &Cx,
    sqlite_path: &str,
    run_migrations: bool,
) -> Outcome<(), SqlError> {
    if run_migrations {
        let mig_conn = crate::guard_db_conn(
            match open_sqlite_file_with_lock_retry(sqlite_path) {
                Ok(conn) => conn,
                Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=open_file failed: {err}"
                    )));
                }
            },
            "sqlite init migration connection",
        );

        if let Err(err) = execute_sql_with_lock_retry(
            &mig_conn,
            sqlite_path,
            schema::PRAGMA_DB_INIT_BASE_SQL,
            "sqlite init base pragmas",
        ) {
            return Outcome::Err(SqlError::Custom(format!(
                "sqlite init stage=base_pragmas failed: {err}"
            )));
        }

        match schema::migrate_to_latest_base(cx, &*mig_conn).await {
            Outcome::Ok(_) => {}
            Outcome::Err(err) => {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=migrate_to_latest_base failed: {err}"
                )));
            }
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }

        drop(mig_conn);

        // Run full migrations (including v10-v15) via canonical SQLite.
        //
        // Base-mode migrations intentionally skip migrations that require
        // features unsupported by FrankenConnection (e.g. ALTER TABLE ADD
        // COLUMN reparsing). Canonical SQLite handles these correctly, so we
        // open a second connection to apply any remaining pending migrations
        // such as v15 (recipients_json column addition).
        let canonical_conn = match open_sqlite_file_with_lock_retry_canonical(sqlite_path) {
            Ok(conn) => conn,
            Err(err) => {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=open_canonical_for_full_migrations failed: {err}"
                )));
            }
        };

        if let Err(err) = canonical_conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL) {
            return Outcome::Err(SqlError::Custom(format!(
                "sqlite init stage=canonical_pragmas failed: {err}"
            )));
        }

        match schema::migrate_to_latest(cx, &canonical_conn).await {
            Outcome::Ok(_) => {}
            Outcome::Err(err) => {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=migrate_to_latest (full) failed: {err}"
                )));
            }
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }

        drop(canonical_conn);
    }

    let runtime_conn = crate::guard_db_conn(
        match open_sqlite_file_with_lock_retry(sqlite_path) {
            Ok(conn) => conn,
            Err(err) => {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=open_file_runtime failed: {err}"
                )));
            }
        },
        "sqlite init runtime connection",
    );

    if !run_migrations
        && let Err(err) = execute_sql_with_lock_retry(
            &runtime_conn,
            sqlite_path,
            schema::PRAGMA_DB_INIT_BASE_SQL,
            "sqlite init runtime base pragmas",
        )
    {
        return Outcome::Err(SqlError::Custom(format!(
            "sqlite init stage=base_pragmas_runtime failed: {err}"
        )));
    }

    // Always enforce startup cleanup for legacy identity FTS artifacts.
    // These can be reintroduced by historical/full migration paths and have
    // caused post-crash rowid/index mismatch failures.
    if let Err(err) = schema::enforce_runtime_fts_cleanup(&runtime_conn) {
        return Outcome::Err(SqlError::Custom(format!(
            "sqlite init stage=enforce_runtime_fts_cleanup failed: {err}"
        )));
    }

    // Switch to WAL journal mode AFTER migrations complete.
    //
    // Migrations run in DELETE (rollback) mode for safety. Runtime connections
    // intentionally do not reissue `journal_mode=WAL`, because that database-
    // wide transition amplifies lock contention on pool acquire and durability
    // probes. Set WAL once here before pooled connections open.
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/13
    if let Err(err) = execute_sql_with_lock_retry(
        &runtime_conn,
        sqlite_path,
        "PRAGMA journal_mode = WAL;",
        "sqlite init switch journal_mode=WAL",
    ) {
        tracing::warn!(
            path = %sqlite_path,
            error = %err,
            "failed to switch journal_mode to WAL after init; runtime will continue in rollback-journal mode until a later init succeeds"
        );
        // Non-fatal: reads/writes can still proceed, but concurrency may degrade.
    }

    // Rebuild inbox_stats from ground truth, drop legacy triggers, and fix
    // mixed-scale timestamps left by the Python server.
    if let Err(err) = startup_data_repairs(&runtime_conn) {
        tracing::warn!(
            path = %sqlite_path,
            error = %err,
            "startup data repairs failed; some counters/timestamps may be stale"
        );
    }

    drop(runtime_conn);
    Outcome::Ok(())
}

/// One-shot data repairs run at startup before pool connections are handed out.
///
/// 1. Drop legacy `inbox_stats` triggers (redundant with explicit rebuilds).
/// 2. Rebuild `inbox_stats` from ground truth.
/// 3. Fix mixed-scale timestamps (seconds/millis → microseconds).
#[allow(clippy::result_large_err)]
fn startup_data_repairs(conn: &DbConn) -> Result<(), SqlError> {
    // ── inbox_stats rebuild ──────────────────────────────────────────
    let has_inbox_stats = !conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='inbox_stats'",
            &[],
        )?
        .is_empty();

    if has_inbox_stats {
        // Drop legacy triggers to prevent double-counting.
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_insert")?;
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_mark_read")?;
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_ack")?;

        // Full rebuild from ground truth.
        conn.execute_raw("DELETE FROM inbox_stats")?;
        conn.execute_raw(
            "INSERT INTO inbox_stats \
                (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
            SELECT \
                r.agent_id, \
                COUNT(*) AS total_count, \
                SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
                SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
                MAX(m.created_ts) AS last_message_ts \
            FROM message_recipients r \
            JOIN messages m ON m.id = r.message_id \
            GROUP BY r.agent_id",
        )?;
    }

    // ── Fix mixed-scale timestamps ───────────────────────────────────
    // The Python server occasionally wrote created_ts in seconds or
    // milliseconds instead of microseconds.  Detect and upscale:
    //   seconds  (< 1e13)  → × 1_000_000
    //   millis   (< 1e16)  → × 1_000
    let has_messages = !conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='messages'",
            &[],
        )?
        .is_empty();

    if has_messages {
        // Magnitude-based detection (non-overlapping ranges):
        //   2026 seconds  ≈ 1.77 × 10⁹   →  < 10¹² is definitely seconds
        //   2026 millis   ≈ 1.77 × 10¹²   →  [10¹², 10¹⁵) is definitely millis
        //   2026 micros   ≈ 1.77 × 10¹⁵   →  ≥ 10¹⁵ is already correct
        //
        // Order matters: handle millis FIRST to avoid seconds check catching
        // millis values that happen to be < 10¹² (they can't — millis are ≥ 10¹²).
        // But we still process millis first as a safety measure.

        // Milliseconds → microseconds  [10^12, 10^15)
        conn.execute_raw(
            "UPDATE messages SET created_ts = created_ts * 1000 \
             WHERE created_ts >= 1000000000000 AND created_ts < 1000000000000000",
        )?;
        // Seconds → microseconds  (0, 10^12)
        conn.execute_raw(
            "UPDATE messages SET created_ts = created_ts * 1000000 \
             WHERE created_ts > 0 AND created_ts < 1000000000000",
        )?;
    }

    Ok(())
}

#[must_use]
fn should_retry_sqlite_init_error(error: &SqlError) -> bool {
    let msg = error.to_string();
    is_sqlite_recovery_error_message(&msg) || is_lock_error(&msg)
}

const SQLITE_LOCK_MAX_RETRIES: usize = 3;

#[must_use]
fn sqlite_lock_retry_delay(retry_index: usize) -> Duration {
    let exponent = u32::try_from(retry_index.min(3)).unwrap_or(3);
    Duration::from_millis(25_u64.saturating_mul(1_u64 << exponent))
}

#[must_use]
pub fn is_sqlite_snapshot_conflict_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("snapshot conflict on pages")
        || lower.contains("busy_snapshot")
        || lower.contains("snapshot too old")
        || (lower.contains("snapshot db_size") && lower.contains("page "))
}

#[must_use]
pub fn is_sqlite_recovery_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_corruption_error_message(message)
        || is_sqlite_snapshot_conflict_error_message(message)
        || lower.contains("out of memory")
        || lower.contains("cursor stack is empty")
        || lower.contains("called `option::unwrap()` on a `none` value")
        || lower.contains("internal error")
        || lower.contains("cursor must be on a leaf")
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

#[allow(clippy::result_large_err)]
fn open_sqlite_file_with_lock_retry(sqlite_path: &str) -> Result<DbConn, SqlError> {
    open_sqlite_file_with_lock_retry_impl(
        sqlite_path,
        |path| DbConn::open_file(path),
        std::thread::sleep,
    )
}

#[allow(clippy::result_large_err)]
fn open_sqlite_file_with_lock_retry_canonical(
    sqlite_path: &str,
) -> Result<sqlmodel_sqlite::SqliteConnection, SqlError> {
    open_sqlite_file_with_lock_retry_impl(
        sqlite_path,
        |path| sqlmodel_sqlite::SqliteConnection::open_file(path),
        std::thread::sleep,
    )
}

#[allow(clippy::result_large_err)]
fn retry_sqlite_lock_impl<T, F, S>(
    sqlite_path: &str,
    operation: &str,
    mut op: F,
    mut sleep_fn: S,
) -> Result<T, SqlError>
where
    F: FnMut() -> Result<T, SqlError>,
    S: FnMut(Duration),
{
    let mut retries = 0usize;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) => {
                let message = err.to_string();
                if !is_lock_error(&message) || retries >= SQLITE_LOCK_MAX_RETRIES {
                    return Err(err);
                }
                let delay = sqlite_lock_retry_delay(retries);
                let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                tracing::warn!(
                    path = %sqlite_path,
                    operation,
                    error = %err,
                    retry = retries + 1,
                    max_retries = SQLITE_LOCK_MAX_RETRIES,
                    delay_ms,
                    "sqlite operation hit lock/busy error; retrying"
                );
                sleep_fn(delay);
                retries += 1;
            }
        }
    }
}

#[allow(clippy::result_large_err)]
fn open_sqlite_file_with_lock_retry_impl<C, F, S>(
    sqlite_path: &str,
    mut open_file: F,
    sleep_fn: S,
) -> Result<C, SqlError>
where
    F: FnMut(&str) -> Result<C, SqlError>,
    S: FnMut(Duration),
{
    retry_sqlite_lock_impl(
        sqlite_path,
        "sqlite open",
        || open_file(sqlite_path),
        sleep_fn,
    )
}

#[allow(clippy::result_large_err)]
fn execute_sql_with_lock_retry(
    conn: &DbConn,
    sqlite_path: &str,
    sql: &str,
    operation: &str,
) -> Result<(), SqlError> {
    retry_sqlite_lock_impl(
        sqlite_path,
        operation,
        || conn.execute_raw(sql),
        std::thread::sleep,
    )
}

/// Open a file-backed sqlite connection and automatically recover from
/// corruption-like open failures when possible.
#[allow(clippy::result_large_err)]
pub fn open_sqlite_file_with_recovery(sqlite_path: &str) -> Result<DbConn, SqlError> {
    if sqlite_path == ":memory:" {
        return DbConn::open_memory();
    }
    ensure_sqlite_parent_dir_exists(sqlite_path)?;

    match open_sqlite_file_with_lock_retry(sqlite_path) {
        Ok(conn) => Ok(conn),
        Err(primary_err) => {
            let primary_msg = primary_err.to_string();

            if let Some(fallback_path) = sqlite_absolute_fallback_path(sqlite_path, &primary_msg) {
                match open_sqlite_file_with_lock_retry(&fallback_path) {
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
            open_sqlite_file_with_lock_retry(sqlite_path).map_err(|reopen_err| {
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
    // Do not run archive-aware recovery before the first real initialization attempt.
    // On live databases this can turn normal startup into an expensive or destructive
    // recovery path before we've observed any concrete corruption signal.

    match run_sqlite_init_once(cx, sqlite_path, run_migrations).await {
        ok @ Outcome::Ok(()) => ok,
        non_err @ (Outcome::Cancelled(_) | Outcome::Panicked(_)) => non_err,
        Outcome::Err(first_err) => {
            if !should_retry_sqlite_init_error(&first_err) {
                return Outcome::Err(first_err);
            }

            if is_sqlite_recovery_error_message(&first_err.to_string()) {
                match sqlite_file_is_healthy(path) {
                    Ok(false) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "sqlite init failed and health probes detected corruption; attempting automatic recovery"
                        );
                        if let Err(recover_err) = recover_sqlite_file(path) {
                            if !should_retry_sqlite_init_error(&recover_err) {
                                return Outcome::Err(recover_err);
                            }
                            tracing::warn!(
                                path = %path.display(),
                                error = %recover_err,
                                "sqlite recovery attempt failed with retryable error; retrying init once"
                            );
                        }
                    }
                    Ok(true) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "sqlite init failed but file-level health probes passed; retrying initialization once"
                        );
                    }
                    Err(health_err) => {
                        if !should_retry_sqlite_init_error(&health_err) {
                            return Outcome::Err(health_err);
                        }
                        tracing::warn!(
                            path = %path.display(),
                            error = %health_err,
                            "sqlite health probe failed with retryable error; retrying initialization once"
                        );
                    }
                }
            } else {
                // Lock/busy class errors are often transient under concurrent startup.
                // Skip corruption probes and retry initialization once.
                tracing::warn!(
                    path = %path.display(),
                    error = %first_err,
                    "sqlite init failed with retryable lock/busy error; retrying initialization once"
                );
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
        || lower.contains("database schema is corrupt")
        || lower.contains("file is not a database")
        || lower.contains("database file too small for header")
        || lower.contains("invalid database header")
        || lower.contains("invalid database header magic")
        || lower.contains("invalid page size")
        || lower.contains("malformed page")
        || lower.contains("page checksum mismatch")
        || lower.contains("header checksum mismatch")
        || lower.contains("no healthy backup was found")
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_details_from_rows(rows: &[sqlmodel_core::Row]) -> Vec<String> {
    let mut details: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(v) = row.get_named::<String>("quick_check") {
            details.push(v);
        } else if let Ok(v) = row.get_named::<String>("integrity_check") {
            details.push(v);
        } else if let Some(Value::Text(v)) = row.values().next() {
            details.push(v.clone());
        }
    }
    if details.is_empty() {
        details.push("ok".to_string());
    }
    details
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_details(conn: &DbConn, pragma_sql: &str) -> Result<Vec<String>, SqlError> {
    let rows = conn.query_sync(pragma_sql, &[])?;
    Ok(sqlite_pragma_check_details_from_rows(&rows))
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_is_ok(conn: &DbConn, pragma_sql: &str) -> Result<bool, SqlError> {
    let details = sqlite_pragma_check_details(conn, pragma_sql)?;
    Ok(details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok")))
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
fn sqlite_pragma_check_details_canonical(
    conn: &sqlmodel_sqlite::SqliteConnection,
    pragma_sql: &str,
) -> Result<Vec<String>, SqlError> {
    let rows = conn.query_sync(pragma_sql, &[])?;
    Ok(sqlite_pragma_check_details_from_rows(&rows))
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_is_ok_canonical(
    conn: &sqlmodel_sqlite::SqliteConnection,
    pragma_sql: &str,
) -> Result<bool, SqlError> {
    let details = sqlite_pragma_check_details_canonical(conn, pragma_sql)?;
    Ok(details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok")))
}

#[allow(clippy::result_large_err)]
fn sqlite_canonical_quick_check_is_ok(
    conn: &sqlmodel_sqlite::SqliteConnection,
) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok_canonical(conn, "PRAGMA quick_check")
}

#[allow(clippy::result_large_err)]
fn sqlite_canonical_incremental_check_is_ok(
    conn: &sqlmodel_sqlite::SqliteConnection,
) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok_canonical(conn, "PRAGMA integrity_check(1)")
}

#[must_use]
fn sqlite_file_has_live_sidecars(path: &Path) -> bool {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = PathBuf::from(sidecar_os);
        if let Ok(meta) = std::fs::metadata(&sidecar_path)
            && meta.len() > 0
        {
            return true;
        }
    }
    false
}

#[allow(clippy::result_large_err)]
fn sqlite_file_is_healthy_canonical(path: &Path) -> Result<bool, SqlError> {
    let path_str = path.to_string_lossy();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(path_str.as_ref())?;

    if !sqlite_canonical_quick_check_is_ok(&conn)? {
        return Ok(false);
    }
    sqlite_canonical_incremental_check_is_ok(&conn)
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
fn sqlite_file_is_healthy_with_compat_probe<F>(
    path: &Path,
    mut compatibility_probe: F,
) -> Result<bool, SqlError>
where
    F: FnMut(&Path) -> Result<bool, SqlError>,
{
    if !path.exists() {
        return Ok(false);
    }
    let path_str = path.to_string_lossy();
    let conn = match open_sqlite_file_with_lock_retry(path_str.as_ref()) {
        Ok(conn) => conn,
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    };

    match sqlite_quick_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_incremental_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_ack_pending_probe_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg)
                || is_sqlite_snapshot_conflict_error_message(&msg)
                || msg.to_ascii_lowercase().contains("out of memory")
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    // FrankenConnection can miss schema faults present in active WAL sidecars.
    // When sidecars exist, run a canonical sqlite probe to avoid false healthy
    // verdicts (e.g. duplicate-index malformed schema in WAL state).
    if sqlite_file_has_live_sidecars(path) {
        match compatibility_probe(path) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => {
                let msg = e.to_string();
                if is_corruption_error_message(&msg) || is_sqlite_recovery_error_message(&msg) {
                    return Ok(false);
                }
                if is_lock_error(&msg) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "sqlite canonical health probe hit lock/busy error; preserving primary health verdict"
                    );
                } else {
                    return Err(e);
                }
            }
        }
    }

    Ok(true)
}

#[allow(clippy::result_large_err)]
fn sqlite_file_is_healthy(path: &Path) -> Result<bool, SqlError> {
    sqlite_file_is_healthy_with_compat_probe(path, sqlite_file_is_healthy_canonical)
}

#[allow(clippy::result_large_err)]
fn refuse_auto_recovery_with_live_sidecars(primary_path: &Path) -> Result<(), SqlError> {
    if !sqlite_file_has_live_sidecars(primary_path) {
        return Ok(());
    }

    Err(SqlError::Custom(format!(
        "refusing automatic sqlite recovery for {} while live WAL/SHM sidecars are present; stop the server and run explicit repair",
        primary_path.display()
    )))
}

#[must_use]
fn is_index_only_integrity_issue(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("wrong # of entries in index")
        || lower.contains("missing from index")
        || lower.contains("rowid") && lower.contains("index")
}

#[must_use]
fn details_are_index_only_issues(details: &[String]) -> bool {
    !details.is_empty()
        && details.iter().all(|detail| {
            !detail.trim().eq_ignore_ascii_case("ok") && is_index_only_integrity_issue(detail)
        })
}

#[allow(clippy::result_large_err)]
fn try_repair_index_only_corruption(primary_path: &Path) -> Result<bool, SqlError> {
    if !primary_path.exists() {
        return Ok(false);
    }
    let path_str = primary_path.to_string_lossy();
    let conn = open_sqlite_file_with_lock_retry_canonical(path_str.as_ref())?;
    let quick_details = sqlite_pragma_check_details_canonical(&conn, "PRAGMA quick_check")?;
    if quick_details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        return Ok(false);
    }
    if !details_are_index_only_issues(&quick_details) {
        return Ok(false);
    }

    tracing::warn!(
        path = %primary_path.display(),
        details = ?quick_details,
        "detected index-only sqlite corruption; attempting in-place REINDEX repair"
    );

    conn.execute_raw("REINDEX;")?;
    let _ = conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);");

    let post_quick = sqlite_pragma_check_details_canonical(&conn, "PRAGMA quick_check")?;
    if !post_quick
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        tracing::warn!(
            path = %primary_path.display(),
            details = ?post_quick,
            "in-place REINDEX completed but quick_check still reports issues"
        );
        return Ok(false);
    }

    let post_incremental =
        sqlite_pragma_check_details_canonical(&conn, "PRAGMA integrity_check(1)")?;
    if !post_incremental
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        tracing::warn!(
            path = %primary_path.display(),
            details = ?post_incremental,
            "in-place REINDEX passed quick_check but failed integrity_check(1)"
        );
        return Ok(false);
    }

    tracing::warn!(
        path = %primary_path.display(),
        "in-place REINDEX repaired index-only sqlite corruption"
    );
    Ok(true)
}

#[allow(clippy::result_large_err)]
fn recover_sqlite_file(primary_path: &Path) -> Result<(), SqlError> {
    let config = mcp_agent_mail_core::Config::from_env();
    let storage_root_path = config.storage_root.as_path();
    if is_real_directory(storage_root_path) {
        return ensure_sqlite_file_healthy_with_archive(primary_path, storage_root_path);
    }
    ensure_sqlite_file_healthy(primary_path)
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
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
    if is_real_file(&bak) {
        let modified = bak
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((0, modified, bak));
    }

    let backup_prefix = format!("{file_name}.backup-");
    let backup_bak_prefix = format!("{file_name}.bak.");
    let recovery_prefix = format!("{file_name}.recovery");
    if let Ok(entries) = std::fs::read_dir(scan_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let priority = if name.starts_with(&backup_bak_prefix) {
                1
            } else if name.starts_with(&backup_prefix) {
                2
            } else if name.starts_with(&recovery_prefix) {
                3
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
            Ok(false) => match sqlite_file_is_healthy_canonical(&candidate) {
                Ok(true) => {
                    tracing::warn!(
                        candidate = %candidate.display(),
                        "sqlite backup candidate failed primary health probe but passed canonical probe; accepting candidate"
                    );
                    return Some(candidate);
                }
                Ok(false) => tracing::warn!(
                    candidate = %candidate.display(),
                    "sqlite backup candidate failed health probes; skipping"
                ),
                Err(e) => tracing::warn!(
                    candidate = %candidate.display(),
                    error = %e,
                    "sqlite backup candidate canonical probe failed; skipping"
                ),
            },
            Err(e) => tracing::warn!(
                candidate = %candidate.display(),
                error = %e,
                "sqlite backup candidate unreadable; skipping"
            ),
        }
    }
    None
}

fn has_quarantined_primary_artifact(primary_path: &Path) -> bool {
    let Some(file_name) = primary_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = primary_path.parent().unwrap_or_else(|| Path::new("."));
    let scan_dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let corrupt_prefix = format!("{file_name}.corrupt-");

    std::fs::read_dir(scan_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|name| name.starts_with(&corrupt_prefix))
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

#[must_use]
pub fn normalize_sqlite_path_for_pool_key(sqlite_path: &str) -> String {
    resolve_sqlite_path_with_absolute_fallback(sqlite_path)
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
fn quarantine_reconstructed_candidate(
    primary_path: &Path,
    timestamp: &str,
    reason: &str,
) -> Result<Option<PathBuf>, SqlError> {
    if !primary_path.exists() {
        return Ok(None);
    }

    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined = primary_path.with_file_name(format!("{base_name}.{reason}-{timestamp}"));
    std::fs::rename(primary_path, &quarantined).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine reconstructed database candidate {}: {e}",
            primary_path.display()
        ))
    })?;

    if let Err(e) = quarantine_sidecar(primary_path, "-wal", timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-wal", primary_path.display()),
            error = %e,
            "failed to quarantine WAL sidecar for reconstructed candidate; continuing"
        );
    }
    if let Err(e) = quarantine_sidecar(primary_path, "-shm", timestamp) {
        tracing::warn!(
            sidecar = %format!("{}-shm", primary_path.display()),
            error = %e,
            "failed to quarantine SHM sidecar for reconstructed candidate; continuing"
        );
    }

    Ok(Some(quarantined))
}

#[allow(clippy::result_large_err)]
fn restore_quarantined_primary(
    primary_path: &Path,
    quarantined_path: &Path,
) -> Result<(), SqlError> {
    if !quarantined_path.exists() {
        return Ok(());
    }

    if primary_path.exists() {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        let _ = quarantine_reconstructed_candidate(
            primary_path,
            &timestamp,
            "archive-reconcile-restore",
        );
    }

    std::fs::rename(quarantined_path, primary_path).map_err(|e| {
        SqlError::Custom(format!(
            "failed to restore original database {} from {}: {e}",
            primary_path.display(),
            quarantined_path.display()
        ))
    })
}

/// Rebuild a healthy-but-stale SQLite file from the archive while salvaging the
/// current primary database for any DB-only state that is not archived.
#[allow(clippy::result_large_err)]
pub fn reconstruct_sqlite_file_with_archive_salvage(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<crate::reconstruct::ReconstructStats, SqlError> {
    if !primary_path.exists() {
        return crate::reconstruct::reconstruct_from_archive(primary_path, storage_root)
            .map_err(|e| SqlError::Custom(e.to_string()));
    }

    if let Ok(conn) = open_sqlite_file_with_recovery(primary_path.to_string_lossy().as_ref()) {
        let _ = conn.query_sync("PRAGMA busy_timeout=60000;", &[]);
        let _ = conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[]);
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined =
        primary_path.with_file_name(format!("{base_name}.archive-reconcile-{timestamp}"));

    std::fs::rename(primary_path, &quarantined).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine database {} for archive reconciliation: {e}",
            primary_path.display()
        ))
    })?;
    let _ = quarantine_sidecar(primary_path, "-wal", &timestamp);
    let _ = quarantine_sidecar(primary_path, "-shm", &timestamp);

    match crate::reconstruct::reconstruct_from_archive_with_salvage(
        primary_path,
        storage_root,
        Some(&quarantined),
    ) {
        Ok(stats) => match sqlite_file_is_healthy(primary_path) {
            Ok(true) => Ok(stats),
            Ok(false) => {
                let _ = quarantine_reconstructed_candidate(
                    primary_path,
                    &timestamp,
                    "archive-reconcile-failed",
                );
                restore_quarantined_primary(primary_path, &quarantined)?;
                Err(SqlError::Custom(format!(
                    "archive reconciliation produced an unhealthy database at {}",
                    primary_path.display()
                )))
            }
            Err(e) => {
                let _ = quarantine_reconstructed_candidate(
                    primary_path,
                    &timestamp,
                    "archive-reconcile-failed",
                );
                restore_quarantined_primary(primary_path, &quarantined)?;
                Err(e)
            }
        },
        Err(e) => {
            let _ = quarantine_reconstructed_candidate(
                primary_path,
                &timestamp,
                "archive-reconcile-failed",
            );
            restore_quarantined_primary(primary_path, &quarantined)?;
            Err(SqlError::Custom(format!(
                "archive reconciliation failed for {}: {e}",
                primary_path.display()
            )))
        }
    }
}

#[allow(clippy::result_large_err)]
fn restore_from_backup(primary_path: &Path, backup_path: &Path) -> Result<(), SqlError> {
    if !is_real_file(backup_path) {
        return Err(SqlError::Custom(format!(
            "refusing to restore sqlite backup from non-regular file {}",
            backup_path.display()
        )));
    }

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
    let _conn = crate::guard_db_conn(
        open_sqlite_file_with_lock_retry(path_str.as_ref()).map_err(|e| {
            SqlError::Custom(format!(
                "failed to initialize fresh sqlite file {}: {e}",
                primary_path.display()
            ))
        })?,
        "scratch sqlite reinit connection",
    );

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
/// 1. Search for a healthy `.bak` / `.bak.*` / `.backup-*` / `.recovery*` sibling.
/// 2. Quarantine the corrupt file (rename to `*.corrupt-{timestamp}`).
/// 3. Restore from the first healthy backup found.
/// 4. If no healthy backup exists, reinitialize an empty database file.
///
/// Returns `Ok(())` when the file at `primary_path` is healthy (either
/// originally or after successful recovery).
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy(primary_path: &Path) -> Result<(), SqlError> {
    let exists = primary_path.exists();
    if exists && sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    if exists {
        refuse_auto_recovery_with_live_sidecars(primary_path)?;
    }
    if exists {
        match try_repair_index_only_corruption(primary_path) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                path = %primary_path.display(),
                error = %e,
                "in-place sqlite index repair probe failed; continuing with standard recovery"
            ),
        }
    }

    if let Some(backup_path) = find_healthy_backup(primary_path) {
        restore_from_backup(primary_path, &backup_path)?;
        if sqlite_file_is_healthy(primary_path)? {
            return Ok(());
        }
        if exists {
            return Err(SqlError::Custom(format!(
                "database file {} was restored from {}, but health probes still failed",
                primary_path.display(),
                backup_path.display()
            )));
        }
        // If missing originally and restore failed, fall through to reinitialize
    } else if !exists {
        // Missing file, no backup. Normal fresh startup.
        return Ok(());
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
/// 1. `.bak` / `.bak.*` / `.backup-*` / `.recovery*` backup files
/// 2. Git archive reconstruction (recovers messages + agents)
/// 3. Blank reinitialization (empty database)
#[allow(clippy::too_many_lines)]
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy_with_archive(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<(), SqlError> {
    let had_primary = primary_path.exists();
    if had_primary && sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    if had_primary {
        refuse_auto_recovery_with_live_sidecars(primary_path)?;
    }
    if had_primary {
        match try_repair_index_only_corruption(primary_path) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                path = %primary_path.display(),
                error = %e,
                "in-place sqlite index repair probe failed; continuing with archive-aware recovery"
            ),
        }
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
    } else if !had_primary {
        // Missing file, no backup.
        if has_quarantined_primary_artifact(primary_path) {
            return Err(SqlError::Custom(format!(
                "database file {} is missing but quarantined corrupt artifact(s) exist; refusing blank reinitialization without operator action",
                primary_path.display()
            )));
        }
        if !is_real_directory(storage_root) || !is_real_directory(&storage_root.join("projects")) {
            // Normal fresh startup (no projects directory).
            return Ok(());
        }
        // Missing file, but archive has projects. We want to reconstruct!
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
                if had_primary && stats.agents == 0 && stats.messages == 0 {
                    if let Some(quarantined) = quarantine_reconstructed_candidate(
                        primary_path,
                        &timestamp,
                        "reconstruct-empty",
                    )? {
                        tracing::warn!(
                            primary = %primary_path.display(),
                            quarantined = %quarantined.display(),
                            "quarantined empty reconstructed database candidate"
                        );
                    }
                    return Err(SqlError::Custom(format!(
                        "database file {} was quarantined for archive-aware recovery, but archive reconstruction restored no durable mail state; refusing blank reinitialization to avoid data loss",
                        primary_path.display()
                    )));
                }
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

    if had_primary {
        if let Some(quarantined) =
            quarantine_reconstructed_candidate(primary_path, &timestamp, "reconstruct-failed")?
        {
            tracing::warn!(
                primary = %primary_path.display(),
                quarantined = %quarantined.display(),
                "quarantined reconstructed database candidate after failed archive recovery"
            );
        }
        return Err(SqlError::Custom(format!(
            "database file {} was quarantined for archive-aware recovery, but reconstruction did not produce a healthy database; refusing blank reinitialization to avoid data loss",
            primary_path.display()
        )));
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
/// the same effective pool signature only take a shared read lock (zero
/// contention on the hot path). The write lock is only held briefly when
/// creating a new pool.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let cache =
        POOL_CACHE.get_or_init(|| OrderedRwLock::new(LockLevel::DbPoolCache, HashMap::new()));
    let cache_key = pool_cache_key(config);

    // Fast path: shared read lock for existing live pool (concurrent readers).
    {
        let guard = cache.read();
        if let Some(pool) = guard.get(&cache_key)
            && let Some(shared_pool) = pool.upgrade()
        {
            return DbPool::from_shared_pool(config, shared_pool);
        }
    }

    // Slow path: exclusive write lock to create a new pool (rare), or to
    // evict dead weak entries left after all callers dropped a pool.
    let mut guard = cache.write();
    // Double-check after acquiring write lock — another thread may have won the race.
    if let Some(pool) = guard.get(&cache_key) {
        if let Some(shared_pool) = pool.upgrade() {
            return DbPool::from_shared_pool(config, shared_pool);
        }
        guard.remove(&cache_key);
    }

    let pool = DbPool::new(config)?;
    guard.insert(cache_key, Arc::downgrade(&pool.pool));
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
    fn normalize_sqlite_identity_path_caches_recent_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("identity_cache.db");
        let raw_path = db_path.to_string_lossy().into_owned();

        sqlite_identity_path_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let first = normalize_sqlite_identity_path(&raw_path);
        let cached = sqlite_identity_path_cache_get(&raw_path);
        assert_eq!(cached.as_deref(), Some(first.as_str()));

        let second = normalize_sqlite_identity_path(&raw_path);
        assert_eq!(second, first);
    }

    #[test]
    fn sqlite_identity_path_cache_entries_expire_after_test_freshness_window() {
        let raw_path = "relative/cache-expiry.db";
        let normalized = "/tmp/cache-expiry.db";

        sqlite_identity_path_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        sqlite_identity_path_cache_insert(raw_path, normalized);
        assert_eq!(
            sqlite_identity_path_cache_get(raw_path).as_deref(),
            Some(normalized)
        );

        std::thread::sleep(SQLITE_IDENTITY_PATH_CACHE_FRESHNESS + Duration::from_millis(10));
        assert!(
            sqlite_identity_path_cache_get(raw_path).is_none(),
            "expired entries should be evicted on read"
        );
    }

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
        assert_eq!(config.sqlite_path().unwrap(), "/relative/path.db");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3?mode=rwc".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3#v1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:///home/ubuntu/storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(
            config.sqlite_path().unwrap(),
            "/home/ubuntu/storage.sqlite3"
        );

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

    #[test]
    fn per_connection_pragmas_omit_db_wide_journal_mode() {
        assert!(
            !schema::PRAGMA_CONN_SETTINGS_SQL.contains("journal_mode"),
            "fresh probe/read connections must not try to switch journal mode"
        );

        let sql = schema::build_conn_pragmas(4);
        assert!(
            !sql.contains("journal_mode"),
            "pool connection init must not reissue journal_mode=WAL: {sql}"
        );
    }

    #[test]
    fn second_pool_acquire_succeeds_under_reserved_lock_after_init() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("second_acquire_reserved_lock.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 2,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let first_conn = rt
            .block_on(async { pool.acquire(&cx).await })
            .into_result()
            .expect("acquire first pooled connection");

        let lock_conn = DbConn::open_file(db_path.display().to_string()).expect("open lock db");
        lock_conn
            .execute_raw("PRAGMA busy_timeout = 1")
            .expect("set lock busy_timeout");
        lock_conn
            .execute_raw("BEGIN IMMEDIATE")
            .expect("hold reserved sqlite lock");

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let pool_for_thread = pool;
        let acquire_thread = std::thread::spawn(move || {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build thread runtime");
            let cx = Cx::for_testing();
            let result = rt.block_on(async {
                match pool_for_thread.acquire(&cx).await {
                    Outcome::Ok(conn) => conn
                        .query_sync("SELECT 1 AS one", &[])
                        .map(|rows| rows.len())
                        .map_err(|e| format!("query via second pooled connection failed: {e}")),
                    Outcome::Err(err) => Err(format!("second pooled acquire failed: {err}")),
                    Outcome::Cancelled(reason) => {
                        Err(format!("second pooled acquire cancelled: {reason:?}"))
                    }
                    Outcome::Panicked(payload) => Err(format!(
                        "second pooled acquire panicked: {}",
                        payload.message()
                    )),
                }
            });
            result_tx.send(result).expect("send acquire result");
        });

        let row_count = match result_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(result) => result.expect("reserved lock should not block second pooled acquire"),
            Err(err) => {
                let _ = lock_conn.execute_raw("ROLLBACK");
                acquire_thread
                    .join()
                    .expect("join acquire thread after timeout");
                panic!("second pooled acquire should not stall under reserved lock: {err}");
            }
        };
        assert_eq!(row_count, 1, "second pooled connection should stay usable");

        lock_conn
            .execute_raw("ROLLBACK")
            .expect("release sqlite lock");
        drop(first_conn);
        acquire_thread.join().expect("join acquire thread");
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
    fn sqlite_backup_candidates_include_timestamped_bak_series() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let backup_bak_series = dir.path().join("storage.sqlite3.bak.20260212_000000");
        let backup_series = dir.path().join("storage.sqlite3.backup-20260212_010000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&backup_bak_series, b"bak series").expect("write .bak timestamp series");
        std::fs::write(&backup_series, b"backup series").expect("write .backup- series");

        let candidates = sqlite_backup_candidates(&primary);
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(backup_bak_series.as_path()),
            "timestamped .bak.* backups should be discovered and prioritized over .backup-*"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_backup_candidates_skip_symlinked_backups() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let real_backup = dir.path().join("outside.sqlite3");
        let symlinked_bak = dir.path().join("storage.sqlite3.bak");
        let symlinked_series = dir.path().join("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&real_backup, b"backup").expect("write real backup");
        symlink(&real_backup, &symlinked_bak).expect("symlink .bak");
        symlink(&real_backup, &symlinked_series).expect("symlink .backup- series");

        let candidates = sqlite_backup_candidates(&primary);
        assert!(
            candidates.is_empty(),
            "symlinked backup candidates must be ignored"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_restores_from_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");

        // Create a healthy DB as a backup.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .unwrap();
        drop(conn);
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));
        std::fs::copy(&primary, &backup).unwrap();

        // Corrupt the primary DB file.
        std::fs::write(&primary, b"corrupted-data").unwrap();

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
    fn ensure_sqlite_file_healthy_restores_from_timestamped_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup1 = dir.path().join("storage.sqlite3.bak.20240101_120000");
        let backup2 = dir.path().join("storage.sqlite3.bak.20240102_120000"); // Should pick the newest

        // Create a healthy DB.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE t (x INTEGER)").unwrap();
        drop(conn);
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));

        // Create dummy older backup and real newer backup (which must be a valid DB!).
        std::fs::write(&backup1, b"corrupted-old-backup").unwrap();
        let conn2 = DbConn::open_file(backup2.to_string_lossy().as_ref()).unwrap();
        conn2
            .execute_raw("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (42);")
            .unwrap();
        drop(conn2);

        // Corrupt the primary to trigger recovery.
        std::fs::write(&primary, b"broken").unwrap();

        ensure_sqlite_file_healthy(&primary).expect("auto-recovery should succeed");

        // Verify the restored DB is exactly the valid backup.
        let restored_conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let val: i64 = restored_conn.query_sync("SELECT x FROM t", &[]).unwrap()[0]
            .get_named("x")
            .unwrap();
        assert_eq!(
            val, 42,
            "restored DB should preserve timestamped backup data"
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
    fn pool_init_preserves_legacy_fixture_rows() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("legacy_fixture.db");
        let db_path_str = db_path.to_string_lossy();

        let seed_conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path_str.as_ref())
            .expect("open seed sqlite db");
        let seed_sql = [
            "PRAGMA foreign_keys = OFF",
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
            "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL, inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto')",
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL, ack_required INTEGER NOT NULL, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            "CREATE TABLE IF NOT EXISTS message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts DATETIME, ack_ts DATETIME, PRIMARY KEY (message_id, agent_id, kind))",
            "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts DATETIME NOT NULL, expires_ts DATETIME NOT NULL, released_ts DATETIME)",
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'legacy-project', '/tmp/legacy-project', '2026-02-24 15:30:00.123456')",
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (1, 1, 'LegacySender', 'python', 'legacy', 'sender', '2026-02-24 15:30:01', '2026-02-24 15:30:02', 'auto', 'auto')",
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (2, 1, 'LegacyReceiver', 'python', 'legacy', 'receiver', '2026-02-24 15:31:01', '2026-02-24 15:31:02', 'auto', 'auto')",
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (1, 1, 1, 'br-28mgh.8.2', 'Legacy migration message', 'from python db', 'high', 1, '2026-02-24 15:32:00.654321', '[]')",
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 2, 'to', NULL, NULL)",
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (1, 1, 1, 'src/legacy/**', 1, 'legacy reservation', '2026-02-24 15:33:00', '2026-12-24 15:33:00', NULL)",
        ];
        for stmt in seed_sql {
            seed_conn.execute_raw(stmt).expect("seed fixture statement");
        }
        drop(seed_conn);

        let pool = DbPool::new(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _ = pool
                .acquire(&cx)
                .await
                .into_result()
                .expect("acquire pool connection");
        });

        assert!(
            sqlite_file_is_healthy_canonical(&db_path).expect("post-init health probe"),
            "legacy fixture should remain healthy after pool init"
        );

        let verify_conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path_str.as_ref())
            .expect("open verify sqlite db");
        for (table, expected) in [
            ("projects", 1_i64),
            ("agents", 2_i64),
            ("messages", 1_i64),
            ("message_recipients", 1_i64),
            ("file_reservations", 1_i64),
        ] {
            let rows = verify_conn
                .query_sync(&format!("SELECT COUNT(*) AS c FROM {table}"), &[])
                .expect("count query");
            let actual = rows
                .first()
                .and_then(|r| r.get_named::<i64>("c").ok())
                .unwrap_or(-1);
            assert_eq!(actual, expected, "{table} row count should be preserved");
        }

        let type_rows = verify_conn
            .query_sync(
                "SELECT typeof(created_at) AS t FROM projects WHERE id = 1",
                &[],
            )
            .expect("projects type query");
        assert_eq!(
            type_rows[0]
                .get_named::<String>("t")
                .expect("projects.created_at typeof"),
            "integer",
            "timestamp migration should convert TEXT project timestamp to INTEGER"
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

    /// Verify `run_startup_integrity_check` treats a missing DB file as
    /// integrity corruption so callers can trigger recovery/initialization.
    #[test]
    fn startup_integrity_check_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool.run_startup_integrity_check();
        assert!(
            matches!(result, Err(DbError::IntegrityCorruption { .. })),
            "missing file should be treated as integrity corruption needing recovery"
        );
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

    /// Verify `get_or_create_pool` returns the same pool for the same cache key.
    #[test]
    fn get_or_create_pool_caches_by_config_signature() {
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
            "get_or_create_pool should return the same Arc<Pool> for the same cache key"
        );
    }

    /// Verify distinct pool sizing does not alias to the same cached pool.
    #[test]
    fn get_or_create_pool_keeps_small_startup_pools_isolated_from_runtime_pools() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache_shape_test.db");
        let database_url = format!("sqlite:///{}", db_path.display());

        let startup_cfg = DbPoolConfig {
            database_url: database_url.clone(),
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        };
        let runtime_cfg = DbPoolConfig {
            database_url,
            min_connections: 25,
            max_connections: 100,
            ..Default::default()
        };

        let startup_pool = get_or_create_pool(&startup_cfg).expect("startup pool");
        let runtime_pool = get_or_create_pool(&runtime_cfg).expect("runtime pool");

        assert!(
            !Arc::ptr_eq(&startup_pool.pool, &runtime_pool.pool),
            "pool cache must not alias startup/worker tiny pools with runtime pool sizing"
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
        assert!(is_corruption_error_message("database schema is corrupt"));
        assert!(is_corruption_error_message("file is not a database"));
        assert!(is_corruption_error_message(
            "database file too small for header: 14 bytes (< 100)"
        ));
        assert!(is_corruption_error_message(
            "page 12: xxh3 page checksum mismatch"
        ));
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
        assert!(is_sqlite_recovery_error_message(
            "database is busy (snapshot conflict on pages: page 4434 > snapshot db_size 4433 (latest: 4433))"
        ));
        assert!(is_sqlite_recovery_error_message("SQLITE_BUSY_SNAPSHOT"));
        assert!(!is_sqlite_recovery_error_message("database is locked"));
        assert!(!is_sqlite_recovery_error_message("table not found"));
    }

    #[test]
    fn index_only_integrity_issue_detection() {
        assert!(is_index_only_integrity_issue(
            "wrong # of entries in index sqlite_autoindex_agents_1"
        ));
        assert!(is_index_only_integrity_issue(
            "row 4107 missing from index idx_agents_last_active_id_desc"
        ));
        assert!(is_index_only_integrity_issue(
            "rowid 42 missing from index some_idx"
        ));
        assert!(!is_index_only_integrity_issue(
            "database disk image is malformed"
        ));
        assert!(!is_index_only_integrity_issue("file is not a database"));
    }

    #[test]
    fn details_are_index_only_issues_requires_all_lines_to_be_index_issues() {
        assert!(details_are_index_only_issues(&[
            "wrong # of entries in index sqlite_autoindex_agents_1".to_string(),
            "row 4108 missing from index idx_agents_last_active_id_desc".to_string(),
        ]));

        assert!(!details_are_index_only_issues(&["ok".to_string()]));
        assert!(!details_are_index_only_issues(&[
            "wrong # of entries in index sqlite_autoindex_agents_1".to_string(),
            "database disk image is malformed".to_string(),
        ]));
        assert!(!details_are_index_only_issues(&[]));
    }

    #[test]
    fn try_repair_index_only_corruption_is_noop_for_healthy_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("healthy_repair_probe.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, last_active_ts INTEGER NOT NULL, UNIQUE(project_id, name))",
        )
        .expect("create");
        conn.execute_raw(
            "CREATE INDEX idx_agents_last_active_id_desc ON agents(last_active_ts DESC, id DESC)",
        )
        .expect("index");
        conn.execute_raw(
            "INSERT INTO agents(id, project_id, name, last_active_ts) VALUES (1, 1, 'agent', 1)",
        )
        .expect("insert");
        drop(conn);

        let repaired = try_repair_index_only_corruption(&path).expect("repair probe");
        assert!(
            !repaired,
            "healthy DB should not trigger in-place REINDEX repair"
        );
        assert!(
            sqlite_file_is_healthy_canonical(&path).expect("canonical health check"),
            "healthy DB should remain canonically healthy after no-op repair probe"
        );
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

    /// Verify `sqlite_file_is_healthy` returns false for non-existent file.
    #[test]
    fn sqlite_file_is_healthy_nonexistent_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.db");
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(
            !healthy,
            "non-existent file should not be considered healthy"
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
    fn sqlite_file_has_live_sidecars_detects_non_empty_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sidecar.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));

        assert!(!sqlite_file_has_live_sidecars(&path));

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        let shm_path = PathBuf::from(shm_os);
        std::fs::write(&shm_path, b"live-sidecar").expect("write sidecar");
        assert!(sqlite_file_has_live_sidecars(&path));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn sqlite_file_is_healthy_with_sidecar_invokes_compat_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compat_probe.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        std::fs::write(PathBuf::from(shm_os), b"live-sidecar").expect("write sidecar");

        let mut probe_called = false;
        let healthy = sqlite_file_is_healthy_with_compat_probe(&path, |_| {
            probe_called = true;
            Ok(true)
        })
        .expect("health check");
        assert!(healthy, "compat probe true should preserve healthy verdict");
        assert!(
            probe_called,
            "compatibility probe must run when sidecars exist"
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn sqlite_file_is_healthy_with_sidecar_accepts_compat_unhealthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compat_unhealthy.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        std::fs::write(PathBuf::from(shm_os), b"live-sidecar").expect("write sidecar");

        let healthy =
            sqlite_file_is_healthy_with_compat_probe(&path, |_| Ok(false)).expect("health check");
        assert!(
            !healthy,
            "compatibility probe failure should mark file unhealthy"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_refuses_auto_recovery_with_live_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let wal = dir.path().join("storage.sqlite3-wal");
        let shm = dir.path().join("storage.sqlite3-shm");
        std::fs::write(&primary, b"not-a-sqlite-db").expect("write corrupt primary");
        std::fs::write(&wal, b"x").expect("write wal");
        std::fs::write(&shm, b"x").expect("write shm");

        let err = ensure_sqlite_file_healthy(&primary).expect_err("must fail closed");
        let message = err.to_string();
        assert!(
            message.contains("refusing automatic sqlite recovery"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_with_archive_refuses_auto_recovery_with_live_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let wal = dir.path().join("storage.sqlite3-wal");
        let shm = dir.path().join("storage.sqlite3-shm");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("mkdir storage root");
        std::fs::write(&primary, b"not-a-sqlite-db").expect("write corrupt primary");
        std::fs::write(&wal, b"x").expect("write wal");
        std::fs::write(&shm, b"x").expect("write shm");

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("must fail closed");
        let message = err.to_string();
        assert!(
            message.contains("refusing automatic sqlite recovery"),
            "unexpected error: {message}"
        );
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

    #[test]
    fn sqlite_identity_key_is_stable_across_pool_clones() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..DbPoolConfig::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let clone = pool.clone();

        assert_eq!(pool.sqlite_identity_key(), clone.sqlite_identity_key());
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
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));
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

    #[cfg(unix)]
    #[test]
    fn archive_recovery_rejects_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let real_storage = dir.path().join("real-storage");
        let storage_root = dir.path().join("storage");

        std::fs::write(&primary, b"corrupted-data").unwrap();

        let proj_dir = real_storage.join("projects").join("test-proj");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"SwiftFox","registered_ts":"2026-01-15T10:00:00"}"#,
        )
        .unwrap();
        symlink(&real_storage, &storage_root).unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("symlinked storage roots must not be trusted for archive recovery");
        let err_text = err.to_string();
        assert!(
            err_text.contains("refusing blank reinitialization to avoid data loss"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "fail-closed recovery should not leave behind a fresh empty database"
        );
    }

    /// Archive-aware recovery must fail closed when a real DB was quarantined
    /// and no backup/archive path can produce a healthy replacement.
    #[test]
    fn archive_recovery_refuses_blank_reinit_with_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        // Corrupt primary, no backup, empty storage.
        std::fs::write(&primary, b"corrupted-data").unwrap();
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("should refuse to blank-reinitialize after quarantining a real DB");
        let err_text = err.to_string();
        assert!(
            err_text.contains("refusing blank reinitialization to avoid data loss"),
            "unexpected error: {err_text}"
        );

        assert!(
            !primary.exists(),
            "primary DB should stay absent after fail-closed recovery"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
            "quarantined corrupt artifact should be preserved for manual recovery"
        );
    }

    #[test]
    fn archive_recovery_missing_primary_with_quarantined_artifact_is_not_treated_as_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::write(
            dir.path()
                .join("storage.sqlite3.corrupt-20260307_000000_000"),
            b"quarantined-corrupt-db",
        )
        .unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("quarantined corrupt artifacts should block blank reinit");
        let err_text = err.to_string();
        assert!(
            err_text.contains("quarantined corrupt artifact"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "recovery must not silently create a fresh DB when only quarantined state exists"
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
    fn recovery_error_detects_snapshot_conflict() {
        assert!(is_sqlite_snapshot_conflict_error_message(
            "database is busy (snapshot conflict on pages: page 4434 > snapshot db_size 4433 (latest: 4433))"
        ));
        assert!(is_sqlite_snapshot_conflict_error_message(
            "BUSY_SNAPSHOT while opening database"
        ));
    }

    #[test]
    fn recovery_error_rejects_non_recovery_messages() {
        assert!(!is_sqlite_recovery_error_message("connection refused"));
        assert!(!is_sqlite_recovery_error_message("timeout"));
        assert!(!is_sqlite_recovery_error_message("no such table"));
        assert!(!is_sqlite_recovery_error_message(""));
    }

    #[test]
    fn sqlite_init_retry_treats_locked_db_as_retryable() {
        let err = SqlError::Custom("database is locked".to_string());
        assert!(
            should_retry_sqlite_init_error(&err),
            "database lock contention should retry during sqlite init"
        );
    }

    #[test]
    fn sqlite_init_retry_rejects_non_retryable_errors() {
        let err = SqlError::Custom("syntax error near SELECT".to_string());
        assert!(
            !should_retry_sqlite_init_error(&err),
            "non-retryable SQL errors must fail fast during sqlite init"
        );
    }

    #[test]
    fn sqlite_open_lock_retry_delay_exponential_and_capped() {
        assert_eq!(sqlite_lock_retry_delay(0), Duration::from_millis(25));
        assert_eq!(sqlite_lock_retry_delay(1), Duration::from_millis(50));
        assert_eq!(sqlite_lock_retry_delay(2), Duration::from_millis(100));
        assert_eq!(sqlite_lock_retry_delay(3), Duration::from_millis(200));
        assert_eq!(
            sqlite_lock_retry_delay(999),
            Duration::from_millis(200),
            "backoff should cap to avoid unbounded startup delay"
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn retry_sqlite_lock_impl_retries_then_succeeds() {
        let attempts = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result = retry_sqlite_lock_impl(
            "ignored.sqlite3",
            "test operation",
            || {
                let next = attempts.get() + 1;
                attempts.set(next);
                if next <= 2 {
                    Err(SqlError::Custom("database is locked".to_string()))
                } else {
                    Ok(())
                }
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(result.is_ok(), "expected success after lock retries");
        assert_eq!(attempts.get(), 3);
        assert_eq!(
            sleep_calls.borrow().as_slice(),
            &[sqlite_lock_retry_delay(0), sqlite_lock_retry_delay(1)]
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn open_sqlite_file_with_lock_retry_retries_then_succeeds() {
        let open_calls = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result = open_sqlite_file_with_lock_retry_impl(
            "ignored",
            |_| {
                let next = open_calls.get() + 1;
                open_calls.set(next);
                if next <= 2 {
                    Err(SqlError::Custom("database is locked".to_string()))
                } else {
                    DbConn::open_memory()
                }
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(result.is_ok(), "expected success after lock retries");
        assert_eq!(open_calls.get(), 3);
        assert_eq!(
            sleep_calls.borrow().as_slice(),
            &[sqlite_lock_retry_delay(0), sqlite_lock_retry_delay(1)]
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn open_sqlite_file_with_lock_retry_does_not_retry_non_lock_errors() {
        let open_calls = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result: Result<DbConn, SqlError> = open_sqlite_file_with_lock_retry_impl(
            "ignored",
            |_| {
                open_calls.set(open_calls.get() + 1);
                Err(SqlError::Custom("malformed database schema".to_string()))
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(
            result.is_err(),
            "expected immediate failure on non-lock error"
        );
        assert_eq!(open_calls.get(), 1, "non-lock errors should not be retried");
        assert!(
            sleep_calls.borrow().is_empty(),
            "non-lock errors should not trigger backoff sleeps"
        );
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

    #[test]
    fn sqlite_init_drops_legacy_agents_lower_name_index_before_runtime_open() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("legacy_agents_lower_name.sqlite3");
        let db_path_str = db_path.to_str().expect("utf8 db path");

        let canonical = open_sqlite_file_with_lock_retry_canonical(db_path_str)
            .expect("open canonical sqlite file");
        canonical
            .execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL)
            .expect("apply canonical pragmas");
        canonical
            .execute_raw(&schema::init_schema_sql_base())
            .expect("initialize base schema");
        canonical
            .execute_raw(
                "CREATE UNIQUE INDEX uq_agents_name_ci \
                 ON agents(lower(name))",
            )
            .expect("create legacy lower(name) partial index");
        drop(canonical);

        match rt.block_on(run_sqlite_init_once(&cx, db_path_str, true)) {
            Outcome::Ok(()) => {}
            Outcome::Err(err) => panic!("sqlite init should repair legacy index: {err}"),
            Outcome::Cancelled(reason) => panic!("sqlite init cancelled unexpectedly: {reason:?}"),
            Outcome::Panicked(payload) => {
                std::panic::panic_any(payload);
            }
        }

        let verify = open_sqlite_file_with_lock_retry_canonical(db_path_str)
            .expect("reopen canonical sqlite file");
        let legacy_rows = verify
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'index' AND name = 'uq_agents_name_ci'",
                &[],
            )
            .expect("query sqlite_master");
        assert!(
            legacy_rows.is_empty(),
            "canonical init should drop legacy lower(name) partial index"
        );
        drop(verify);

        let runtime = open_sqlite_file_with_lock_retry(db_path_str)
            .expect("runtime sqlite should open after legacy index cleanup");
        let rows = runtime
            .query_sync("SELECT 1 AS val", &[])
            .expect("runtime query");
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
