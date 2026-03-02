//! Error types for the database layer

use thiserror::Error;

/// Database error types
#[derive(Error, Debug)]
pub enum DbError {
    /// `SQLite` error from underlying driver
    #[error("SQLite error: {0}")]
    Sqlite(String),

    /// Connection pool error
    #[error("Pool error: {0}")]
    Pool(String),

    /// Database connection pool exhausted (all connections in use, timeout expired).
    ///
    /// Maps to legacy error code `DATABASE_POOL_EXHAUSTED`.
    #[error("Database connection pool exhausted: {message}")]
    PoolExhausted {
        message: String,
        pool_size: usize,
        max_overflow: usize,
    },

    /// Resource is temporarily busy (lock contention, `SQLITE_BUSY`).
    ///
    /// Maps to legacy error code `RESOURCE_BUSY`.
    #[error("Resource temporarily busy: {0}")]
    ResourceBusy(String),

    /// Circuit breaker is open — database experiencing sustained failures.
    ///
    /// Maps to legacy behavior: fail fast for 30s after 5 consecutive failures.
    #[error("Circuit breaker open: {message}")]
    CircuitBreakerOpen {
        message: String,
        failures: u32,
        reset_after_secs: f64,
    },

    /// Record not found
    #[error("{entity} not found: {identifier}")]
    NotFound {
        entity: &'static str,
        identifier: String,
    },

    /// Duplicate record
    #[error("{entity} already exists: {identifier}")]
    Duplicate {
        entity: &'static str,
        identifier: String,
    },

    /// Invalid argument
    #[error("Invalid {field}: {message}")]
    InvalidArgument {
        field: &'static str,
        message: String,
    },

    /// Schema/migration error
    #[error("Schema error: {0}")]
    Schema(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// `SQLite` integrity check detected corruption.
    #[error("Integrity check failed: {message}")]
    IntegrityCorruption {
        message: String,
        /// The raw output from `PRAGMA integrity_check` / `PRAGMA quick_check`.
        details: Vec<String>,
    },

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Result type alias for database operations
pub type DbResult<T> = std::result::Result<T, DbError>;

impl DbError {
    /// Create a not found error
    pub fn not_found(entity: &'static str, identifier: impl Into<String>) -> Self {
        Self::NotFound {
            entity,
            identifier: identifier.into(),
        }
    }

    /// Create a duplicate error
    pub fn duplicate(entity: &'static str, identifier: impl Into<String>) -> Self {
        Self::Duplicate {
            entity,
            identifier: identifier.into(),
        }
    }

    /// Create an invalid argument error
    pub fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field,
            message: message.into(),
        }
    }

    /// Whether this error indicates a retryable lock/busy condition.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Sqlite(msg) | Self::Pool(msg) | Self::ResourceBusy(msg) => is_lock_error(msg),
            Self::PoolExhausted { .. } => true,
            _ => false,
        }
    }

    /// The legacy error code string for this error.
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        match self {
            Self::PoolExhausted { .. } => "DATABASE_POOL_EXHAUSTED",
            Self::ResourceBusy(_) | Self::CircuitBreakerOpen { .. } => "RESOURCE_BUSY",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::Duplicate { .. } => "DUPLICATE",
            Self::InvalidArgument { .. } => "INVALID_ARGUMENT",
            Self::IntegrityCorruption { .. } => "INTEGRITY_CORRUPTION",
            _ => "INTERNAL_ERROR",
        }
    }

    /// Whether the error is recoverable (client can retry).
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::PoolExhausted { .. }
                | Self::ResourceBusy(_)
                | Self::CircuitBreakerOpen { .. }
                | Self::Pool(_)
        )
    }
}

/// Check whether an error message indicates a database lock/busy condition.
#[must_use]
pub fn is_lock_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("database is locked")
        || lower.contains("database table is locked")
        || lower.contains("database schema is locked")
        || lower.contains("database is busy")
        || lower.contains("locked by another process")
        || lower.contains("unable to open database")
        || lower.contains("disk i/o error")
        || is_mvcc_conflict(msg)
}

/// Check whether an error message indicates an MVCC write conflict
/// (frankensqlite `BEGIN CONCURRENT` page-level collision).
#[must_use]
pub fn is_mvcc_conflict(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("write conflict on page")
        || lower.contains("serialization failure")
        || lower.contains("snapshot too old")
}

/// Check whether an error message indicates pool exhaustion.
#[must_use]
pub fn is_pool_exhausted_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    (lower.contains("pool") && (lower.contains("timeout") || lower.contains("exhausted")))
        || lower.contains("queuepool")
}

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constructor helpers ──────────────────────────────────────────

    #[test]
    fn not_found_constructor() {
        let e = DbError::not_found("Agent", "BlueLake");
        assert!(matches!(
            e,
            DbError::NotFound {
                entity: "Agent",
                ..
            }
        ));
        assert!(e.to_string().contains("BlueLake"));
    }

    #[test]
    fn duplicate_constructor() {
        let e = DbError::duplicate("Project", "/tmp/proj");
        assert!(matches!(
            e,
            DbError::Duplicate {
                entity: "Project",
                ..
            }
        ));
        assert!(e.to_string().contains("/tmp/proj"));
    }

    #[test]
    fn invalid_argument_constructor() {
        let e = DbError::invalid("name", "must be adjective+noun");
        assert!(matches!(e, DbError::InvalidArgument { field: "name", .. }));
        assert!(e.to_string().contains("must be adjective+noun"));
    }

    // ── error_code ──────────────────────────────────────────────────

    #[test]
    fn error_code_pool_exhausted() {
        let e = DbError::PoolExhausted {
            message: "test".into(),
            pool_size: 5,
            max_overflow: 10,
        };
        assert_eq!(e.error_code(), "DATABASE_POOL_EXHAUSTED");
    }

    #[test]
    fn error_code_resource_busy() {
        let e = DbError::ResourceBusy("busy".into());
        assert_eq!(e.error_code(), "RESOURCE_BUSY");
    }

    #[test]
    fn error_code_circuit_breaker() {
        let e = DbError::CircuitBreakerOpen {
            message: "open".into(),
            failures: 5,
            reset_after_secs: 30.0,
        };
        assert_eq!(e.error_code(), "RESOURCE_BUSY");
    }

    #[test]
    fn error_code_not_found() {
        let e = DbError::not_found("X", "y");
        assert_eq!(e.error_code(), "NOT_FOUND");
    }

    #[test]
    fn error_code_duplicate() {
        let e = DbError::duplicate("X", "y");
        assert_eq!(e.error_code(), "DUPLICATE");
    }

    #[test]
    fn error_code_invalid_argument() {
        let e = DbError::invalid("f", "bad");
        assert_eq!(e.error_code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn error_code_integrity_corruption() {
        let e = DbError::IntegrityCorruption {
            message: "bad page".into(),
            details: vec!["page 42".into()],
        };
        assert_eq!(e.error_code(), "INTEGRITY_CORRUPTION");
    }

    #[test]
    fn error_code_internal_variants() {
        // Sqlite, Pool, Schema, Serialization, Internal all map to INTERNAL_ERROR
        for e in [
            DbError::Sqlite("err".into()),
            DbError::Pool("err".into()),
            DbError::Schema("err".into()),
            DbError::Serialization("err".into()),
            DbError::Internal("err".into()),
        ] {
            assert_eq!(e.error_code(), "INTERNAL_ERROR", "for {e}");
        }
    }

    // ── is_retryable ────────────────────────────────────────────────

    #[test]
    fn retryable_pool_exhausted() {
        let e = DbError::PoolExhausted {
            message: "timeout".into(),
            pool_size: 3,
            max_overflow: 0,
        };
        assert!(e.is_retryable());
    }

    #[test]
    fn retryable_resource_busy_with_lock_msg() {
        let e = DbError::ResourceBusy("database is locked".into());
        assert!(e.is_retryable());
    }

    #[test]
    fn retryable_sqlite_locked() {
        let e = DbError::Sqlite("database is locked".into());
        assert!(e.is_retryable());
    }

    #[test]
    fn not_retryable_sqlite_syntax() {
        let e = DbError::Sqlite("syntax error near SELECT".into());
        assert!(!e.is_retryable());
    }

    #[test]
    fn not_retryable_not_found() {
        let e = DbError::not_found("Agent", "x");
        assert!(!e.is_retryable());
    }

    #[test]
    fn not_retryable_duplicate() {
        let e = DbError::duplicate("Agent", "x");
        assert!(!e.is_retryable());
    }

    #[test]
    fn not_retryable_invalid() {
        let e = DbError::invalid("f", "bad");
        assert!(!e.is_retryable());
    }

    // ── is_recoverable ──────────────────────────────────────────────

    #[test]
    fn recoverable_variants() {
        assert!(
            DbError::PoolExhausted {
                message: "x".into(),
                pool_size: 1,
                max_overflow: 0
            }
            .is_recoverable()
        );
        assert!(DbError::ResourceBusy("x".into()).is_recoverable());
        assert!(
            DbError::CircuitBreakerOpen {
                message: "x".into(),
                failures: 1,
                reset_after_secs: 1.0
            }
            .is_recoverable()
        );
        assert!(DbError::Pool("x".into()).is_recoverable());
    }

    #[test]
    fn not_recoverable_variants() {
        assert!(!DbError::not_found("X", "y").is_recoverable());
        assert!(!DbError::duplicate("X", "y").is_recoverable());
        assert!(!DbError::invalid("f", "m").is_recoverable());
        assert!(!DbError::Sqlite("err".into()).is_recoverable());
        assert!(!DbError::Schema("err".into()).is_recoverable());
        assert!(!DbError::Internal("err".into()).is_recoverable());
    }

    // ── is_lock_error ───────────────────────────────────────────────

    #[test]
    fn lock_error_patterns() {
        assert!(is_lock_error("database is locked"));
        assert!(is_lock_error("Database Is Locked")); // case-insensitive
        assert!(is_lock_error("database table is locked: messages"));
        assert!(is_lock_error("database schema is locked"));
        assert!(is_lock_error("database is busy"));
        assert!(is_lock_error("file locked by another process"));
        assert!(is_lock_error("unable to open database file"));
        assert!(is_lock_error("disk I/O error"));
    }

    #[test]
    fn not_lock_error() {
        assert!(!is_lock_error("syntax error"));
        assert!(!is_lock_error("table not found"));
        assert!(!is_lock_error("unlocked and healthy"));
        assert!(!is_lock_error(""));
    }

    // ── is_mvcc_conflict ────────────────────────────────────────────

    #[test]
    fn mvcc_conflict_patterns() {
        assert!(is_mvcc_conflict(
            "write conflict on page 42: held by transaction 7"
        ));
        assert!(is_mvcc_conflict(
            "serialization failure: page 5 was modified after snapshot"
        ));
        assert!(is_mvcc_conflict(
            "snapshot too old: transaction 3 is below GC horizon"
        ));
    }

    #[test]
    fn mvcc_conflict_is_retryable() {
        let e = DbError::Sqlite("write conflict on page 42: held by transaction 7".into());
        assert!(e.is_retryable());
        let e2 =
            DbError::Sqlite("serialization failure: page 5 was modified after snapshot".into());
        assert!(e2.is_retryable());
    }

    #[test]
    fn not_mvcc_conflict() {
        assert!(!is_mvcc_conflict("syntax error"));
        assert!(!is_mvcc_conflict("unique constraint violated"));
        assert!(!is_mvcc_conflict(""));
    }

    // ── is_pool_exhausted_error ─────────────────────────────────────

    #[test]
    fn pool_exhausted_patterns() {
        assert!(is_pool_exhausted_error("pool timeout after 30s"));
        assert!(is_pool_exhausted_error("connection pool exhausted"));
        assert!(is_pool_exhausted_error("QueuePool limit reached"));
    }

    #[test]
    fn not_pool_exhausted() {
        assert!(!is_pool_exhausted_error("database is locked"));
        assert!(!is_pool_exhausted_error("pool party")); // "pool" alone isn't enough
        assert!(!is_pool_exhausted_error(""));
    }

    // ── From<serde_json::Error> ─────────────────────────────────────

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<i32>("invalid").unwrap_err();
        let db_err: DbError = json_err.into();
        assert!(matches!(db_err, DbError::Serialization(_)));
        assert_eq!(db_err.error_code(), "INTERNAL_ERROR");
    }

    // ── Display ─────────────────────────────────────────────────────

    #[test]
    fn display_messages_are_informative() {
        let cases: Vec<(DbError, &str)> = vec![
            (DbError::Sqlite("oops".into()), "SQLite error: oops"),
            (DbError::Pool("gone".into()), "Pool error: gone"),
            (DbError::not_found("Agent", "X"), "Agent not found: X"),
            (
                DbError::duplicate("Project", "/tmp"),
                "Project already exists: /tmp",
            ),
            (DbError::invalid("name", "bad"), "Invalid name: bad"),
            (DbError::Schema("v3 fail".into()), "Schema error: v3 fail"),
            (DbError::Internal("bug".into()), "Internal error: bug"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }
}
