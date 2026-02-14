//! Database layer for MCP Agent Mail
//!
//! This crate provides:
//! - `SQLite` database operations via `sqlmodel_rust`
//! - Connection pooling
//! - Schema migrations
//! - FTS5 full-text search
//!
//! # Timestamp Convention
//!
//! All timestamps are stored as `i64` (microseconds since Unix epoch) internally.
//! This matches `sqlmodel_rust`'s convention. Helper functions are provided to convert
//! to/from `chrono::NaiveDateTime` for API compatibility.

#![forbid(unsafe_code)]

pub mod cache;
pub mod coalesce;
pub mod error;
pub mod integrity;
pub mod mail_explorer;
pub mod models;
pub mod pool;
pub mod queries;
pub mod retry;
pub mod s3fifo;
pub mod schema;
pub mod search_planner;
pub mod search_recipes;
pub mod search_scope;
pub mod search_service;
pub mod search_v3;
pub mod timestamps;
pub mod tracking;

pub use cache::{CacheEntryCounts, CacheMetrics, CacheMetricsSnapshot, cache_metrics, read_cache};
pub use coalesce::{CoalesceMap, CoalesceMetrics, CoalesceOutcome};
pub use error::{DbError, DbResult, is_lock_error, is_pool_exhausted_error};
pub use integrity::{
    CheckKind, IntegrityCheckResult, IntegrityMetrics, attempt_vacuum_recovery, full_check,
    incremental_check, integrity_metrics, is_full_check_due, quick_check,
};
pub use models::*;
pub use pool::{DbPool, DbPoolConfig, auto_pool_size, create_pool, get_or_create_pool};
pub use retry::{
    CIRCUIT_BREAKER, CIRCUIT_DB, CIRCUIT_GIT, CIRCUIT_LLM, CIRCUIT_SIGNAL, CircuitBreaker,
    CircuitState, DbHealthStatus, RetryConfig, Subsystem, SubsystemCircuitStatus, circuit_for,
    db_health_status, retry_sync,
};
pub use timestamps::{
    ClockSkewMetrics, clock_skew_metrics, clock_skew_reset, iso_to_micros, micros_to_iso,
    micros_to_naive, naive_to_micros, now_micros, now_micros_raw,
};
pub use tracking::{
    ActiveTrackerGuard, QueryTracker, QueryTrackerSnapshot, SlowQueryEntry, TableId,
    active_tracker, elapsed_us, query_timer, record_query, set_active_tracker,
};

/// Global query tracker instance.
///
/// Disabled by default (zero overhead). Call `QUERY_TRACKER.enable(threshold_ms)`
/// at startup when `config.instrumentation_enabled` is true.
pub static QUERY_TRACKER: std::sync::LazyLock<QueryTracker> =
    std::sync::LazyLock::new(QueryTracker::new);

// Re-export sqlmodel for convenience
pub use mcp_agent_mail_search_core::{QueryAssistance, parse_query_assistance};
pub use sqlmodel;
pub use sqlmodel_frankensqlite;
pub use sqlmodel_sqlite;

/// The connection type used by this crate's pool and queries.
///
/// We use `SqliteConnection` for runtime durability and parity with CLI/share
/// tooling that also operates through the C-backed `SQLite` driver.
pub type DbConn = sqlmodel_sqlite::SqliteConnection;
