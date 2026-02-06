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

pub mod error;
pub mod models;
pub mod pool;
pub mod queries;
pub mod schema;
pub mod timestamps;
pub mod tracking;

pub use error::{DbError, DbResult};
pub use models::*;
pub use pool::{DbPool, DbPoolConfig, create_pool, get_or_create_pool};
pub use timestamps::{iso_to_micros, micros_to_iso, micros_to_naive, naive_to_micros, now_micros};
pub use tracking::{
    ActiveTrackerGuard, QueryTracker, QueryTrackerSnapshot, SlowQueryEntry, active_tracker,
    elapsed_us, query_timer, set_active_tracker,
};

/// Global query tracker instance.
///
/// Disabled by default (zero overhead). Call `QUERY_TRACKER.enable(threshold_ms)`
/// at startup when `config.instrumentation_enabled` is true.
pub static QUERY_TRACKER: std::sync::LazyLock<QueryTracker> =
    std::sync::LazyLock::new(QueryTracker::new);

// Re-export sqlmodel for convenience
pub use sqlmodel;
pub use sqlmodel_sqlite;
