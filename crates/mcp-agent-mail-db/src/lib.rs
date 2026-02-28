//! Database layer for MCP Agent Mail
//!
//! This crate provides:
//! - `SQLite` database operations via `sqlmodel` on frankensqlite
//! - Connection pooling
//! - Schema migrations
//! - FTS5 full-text search
//!
//! # Timestamp Convention
//!
//! All timestamps are stored as `i64` (microseconds since Unix epoch) internally.
//! This matches `sqlmodel`'s convention. Helper functions are provided to convert
//! to/from `chrono::NaiveDateTime` for API compatibility.

#![forbid(unsafe_code)]

pub mod cache;
pub mod coalesce;
pub mod error;
pub mod integrity;
pub mod mail_explorer;
pub mod migrate;
pub mod models;
pub mod pool;
pub mod queries;
pub mod query_assistance;
pub mod reconstruct;
pub mod retry;
pub mod s3fifo;
pub mod schema;
pub mod search_cache;
pub mod search_candidates;
pub mod search_canonical;
pub mod search_consistency;
pub mod search_diversity;
pub mod search_envelope;
pub mod search_filter_compiler;
pub mod search_planner;
pub mod search_recipes;
pub mod search_response;
pub mod search_rollout;
pub mod search_scope;
pub mod search_service;
pub mod search_updater;
pub mod search_v3;
pub mod sync;
#[cfg(feature = "tantivy-engine")]
pub mod tantivy_schema;
pub mod timestamps;
pub mod tracking;

pub use cache::{CacheEntryCounts, CacheMetrics, CacheMetricsSnapshot, cache_metrics, read_cache};
pub use coalesce::{CoalesceMap, CoalesceMetrics, CoalesceOutcome};
pub use error::{DbError, DbResult, is_lock_error, is_pool_exhausted_error};
pub use integrity::{
    CheckKind, IntegrityCheckResult, IntegrityMetrics, attempt_vacuum_recovery, full_check,
    incremental_check, integrity_metrics, is_full_check_due, quick_check,
};
pub use migrate::{
    ColumnConversionResult, MigrationError, MigrationSummary, TIMESTAMP_COLUMNS, TimestampFormat,
    convert_all_timestamps, convert_column, copy_python_database_to_rust, detect_column_format,
    detect_timestamp_format, find_python_database, text_to_micros,
};
pub use models::*;
pub use pool::{
    DbPool, DbPoolConfig, auto_pool_size, create_pool, ensure_sqlite_file_healthy,
    ensure_sqlite_file_healthy_with_archive, get_or_create_pool, is_corruption_error_message,
    is_sqlite_recovery_error_message, open_sqlite_file_with_recovery,
};
pub use queries::{MvccRetryMetrics, mvcc_retry_metrics};
pub use reconstruct::{ReconstructStats, reconstruct_from_archive};
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

// Re-export search types for consumers
pub use query_assistance::{QueryAssistance, parse_query_assistance};
pub use sqlmodel;
pub use sqlmodel_core;
pub use sqlmodel_frankensqlite;

/// The connection type used by this crate's pool and queries.
///
/// Runtime DB traffic uses `FrankenConnection` to enable pure-Rust `SQLite` with
/// `BEGIN CONCURRENT` write paths.
pub type DbConn = sqlmodel_frankensqlite::FrankenConnection;
