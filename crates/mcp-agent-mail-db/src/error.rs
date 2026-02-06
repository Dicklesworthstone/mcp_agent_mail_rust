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
        || lower.contains("database is busy")
        || lower.contains("locked")
        || lower.contains("unable to open database")
        || lower.contains("disk i/o error")
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
