//! Error types for MCP Agent Mail
//!
//! These error types map to the error categories from the legacy Python codebase.

use thiserror::Error;

/// Result type alias for MCP Agent Mail operations
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for MCP Agent Mail
#[derive(Debug, Error)]
pub enum Error {
    // ==========================================================================
    // Resource Not Found Errors
    // ==========================================================================
    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Message not found: {0}")]
    MessageNotFound(i64),

    #[error("Thread not found: {0}")]
    ThreadNotFound(String),

    #[error("File reservation not found: {0}")]
    ReservationNotFound(i64),

    #[error("Product not found: {0}")]
    ProductNotFound(String),

    // ==========================================================================
    // Validation Errors
    // ==========================================================================
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Invalid agent name: {0}. Must be adjective+noun format (e.g., GreenLake)")]
    InvalidAgentName(String),

    #[error("Invalid thread ID: {0}. Must match ^[A-Za-z0-9][A-Za-z0-9._-]{{0,127}}$")]
    InvalidThreadId(String),

    #[error("Invalid project key: {0}. Must be absolute path")]
    InvalidProjectKey(String),

    #[error("Missing required field: {0}")]
    MissingField(String),

    #[error("Type error: {0}")]
    TypeError(String),

    // ==========================================================================
    // Contact/Authorization Errors
    // ==========================================================================
    #[error("Contact required: {from} -> {to}")]
    ContactRequired { from: String, to: String },

    #[error("Contact blocked: {from} -> {to}")]
    ContactBlocked { from: String, to: String },

    #[error("Capability denied: {0}")]
    CapabilityDenied(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    // ==========================================================================
    // Resource Conflict Errors
    // ==========================================================================
    #[error("File reservation conflict on pattern '{pattern}'. Held by: {holders:?}")]
    ReservationConflict {
        pattern: String,
        holders: Vec<String>,
    },

    #[error("Resource busy: {0}")]
    ResourceBusy(String),

    #[error("Resource exhausted: {0}")]
    ResourceExhausted(String),

    // ==========================================================================
    // Database Errors
    // ==========================================================================
    #[error("Database error: {0}")]
    Database(String),

    #[error("Database pool exhausted")]
    DatabasePoolExhausted,

    #[error("Database lock timeout")]
    DatabaseLockTimeout,

    // ==========================================================================
    // Git/Archive Errors
    // ==========================================================================
    #[error("Git error: {0}")]
    Git(String),

    #[error("Git index lock held by another process")]
    GitIndexLock,

    #[error("Archive lock timeout for project: {0}")]
    ArchiveLockTimeout(String),

    // ==========================================================================
    // I/O Errors
    // ==========================================================================
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    // ==========================================================================
    // Timeout/Cancellation
    // ==========================================================================
    #[error("Operation timed out: {0}")]
    Timeout(String),

    #[error("Operation cancelled")]
    Cancelled,

    // ==========================================================================
    // Connection Errors
    // ==========================================================================
    #[error("Connection error: {0}")]
    Connection(String),

    // ==========================================================================
    // Internal Errors
    // ==========================================================================
    #[error("Internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Returns the error type string (for JSON responses)
    #[must_use]
    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::ProjectNotFound(_)
            | Self::AgentNotFound(_)
            | Self::MessageNotFound(_)
            | Self::ThreadNotFound(_)
            | Self::ReservationNotFound(_)
            | Self::ProductNotFound(_) => "NOT_FOUND",
            Self::InvalidArgument(_)
            | Self::InvalidAgentName(_)
            | Self::InvalidThreadId(_)
            | Self::InvalidProjectKey(_) => "INVALID_ARGUMENT",
            Self::MissingField(_) => "MISSING_FIELD",
            Self::TypeError(_) | Self::Serialization(_) => "TYPE_ERROR",
            Self::ContactRequired { .. } => "CONTACT_REQUIRED",
            Self::ContactBlocked { .. } => "CONTACT_BLOCKED",
            Self::CapabilityDenied(_) => "CAPABILITY_DENIED",
            Self::PermissionDenied(_) => "PERMISSION_ERROR",
            Self::ReservationConflict { .. } | Self::ResourceBusy(_) => "RESOURCE_BUSY",
            Self::ResourceExhausted(_) => "RESOURCE_EXHAUSTED",
            Self::Database(_) | Self::DatabaseLockTimeout => "DATABASE_ERROR",
            Self::DatabasePoolExhausted => "DATABASE_POOL_EXHAUSTED",
            Self::GitIndexLock => "GIT_INDEX_LOCK",
            Self::Git(_) | Self::Internal(_) => "UNHANDLED_EXCEPTION",
            Self::ArchiveLockTimeout(_) => "ARCHIVE_LOCK_TIMEOUT",
            Self::Timeout(_) | Self::Cancelled => "TIMEOUT",
            Self::Io(_) => "OS_ERROR",
            Self::Connection(_) => "CONNECTION_ERROR",
        }
    }

    /// Returns whether the error is recoverable (can be retried)
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            // User-correctable input issues
            Self::ProjectNotFound(_)
                | Self::AgentNotFound(_)
                | Self::MessageNotFound(_)
                | Self::ThreadNotFound(_)
                | Self::ReservationNotFound(_)
                | Self::ProductNotFound(_)
                | Self::InvalidArgument(_)
                | Self::InvalidAgentName(_)
                | Self::InvalidThreadId(_)
                | Self::InvalidProjectKey(_)
                | Self::MissingField(_)
                | Self::TypeError(_)
                | Self::Serialization(_)
                // Coordination / policy
                | Self::ContactRequired { .. }
                | Self::ContactBlocked { .. }
                // Transient / retryable infrastructure
                | Self::Database(_)
                | Self::DatabasePoolExhausted
                | Self::DatabaseLockTimeout
                | Self::GitIndexLock
                | Self::ArchiveLockTimeout(_)
                | Self::ReservationConflict { .. }
                | Self::ResourceBusy(_)
                | Self::ResourceExhausted(_)
                | Self::Timeout(_)
                | Self::Cancelled
                | Self::Connection(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive test: every Error variant maps to the correct `error_type` string.
    #[test]
    fn test_error_type_mapping_exhaustive() {
        let cases: Vec<(Error, &str)> = vec![
            // NOT_FOUND
            (Error::ProjectNotFound("x".into()), "NOT_FOUND"),
            (Error::AgentNotFound("x".into()), "NOT_FOUND"),
            (Error::MessageNotFound(1), "NOT_FOUND"),
            (Error::ThreadNotFound("x".into()), "NOT_FOUND"),
            (Error::ReservationNotFound(1), "NOT_FOUND"),
            (Error::ProductNotFound("x".into()), "NOT_FOUND"),
            // INVALID_ARGUMENT
            (Error::InvalidArgument("x".into()), "INVALID_ARGUMENT"),
            (Error::InvalidAgentName("x".into()), "INVALID_ARGUMENT"),
            (Error::InvalidThreadId("x".into()), "INVALID_ARGUMENT"),
            (Error::InvalidProjectKey("x".into()), "INVALID_ARGUMENT"),
            // MISSING_FIELD
            (Error::MissingField("x".into()), "MISSING_FIELD"),
            // TYPE_ERROR
            (Error::TypeError("x".into()), "TYPE_ERROR"),
            // CONTACT_REQUIRED / CONTACT_BLOCKED
            (
                Error::ContactRequired {
                    from: "a".into(),
                    to: "b".into(),
                },
                "CONTACT_REQUIRED",
            ),
            (
                Error::ContactBlocked {
                    from: "a".into(),
                    to: "b".into(),
                },
                "CONTACT_BLOCKED",
            ),
            // CAPABILITY_DENIED / PERMISSION_ERROR
            (Error::CapabilityDenied("x".into()), "CAPABILITY_DENIED"),
            (Error::PermissionDenied("x".into()), "PERMISSION_ERROR"),
            // RESOURCE_BUSY
            (
                Error::ReservationConflict {
                    pattern: "x".into(),
                    holders: vec![],
                },
                "RESOURCE_BUSY",
            ),
            (Error::ResourceBusy("x".into()), "RESOURCE_BUSY"),
            // RESOURCE_EXHAUSTED
            (Error::ResourceExhausted("x".into()), "RESOURCE_EXHAUSTED"),
            // DATABASE_ERROR
            (Error::Database("x".into()), "DATABASE_ERROR"),
            (Error::DatabaseLockTimeout, "DATABASE_ERROR"),
            // DATABASE_POOL_EXHAUSTED
            (Error::DatabasePoolExhausted, "DATABASE_POOL_EXHAUSTED"),
            // GIT_INDEX_LOCK
            (Error::GitIndexLock, "GIT_INDEX_LOCK"),
            // ARCHIVE_LOCK_TIMEOUT (distinct from TIMEOUT)
            (
                Error::ArchiveLockTimeout("x".into()),
                "ARCHIVE_LOCK_TIMEOUT",
            ),
            // UNHANDLED_EXCEPTION
            (Error::Git("x".into()), "UNHANDLED_EXCEPTION"),
            (Error::Internal("x".into()), "UNHANDLED_EXCEPTION"),
            // TIMEOUT
            (Error::Timeout("x".into()), "TIMEOUT"),
            (Error::Cancelled, "TIMEOUT"),
            // OS_ERROR
            (Error::Io(std::io::Error::other("x")), "OS_ERROR"),
            // CONNECTION_ERROR
            (Error::Connection("x".into()), "CONNECTION_ERROR"),
        ];

        for (err, expected_type) in &cases {
            assert_eq!(
                err.error_type(),
                *expected_type,
                "Error {err:?} should map to {expected_type}"
            );
        }
    }

    /// Exhaustive test: recoverable classification matches legacy Python behavior.
    #[test]
    fn test_recoverable_classification_exhaustive() {
        // Recoverable errors (true)
        let recoverable = vec![
            Error::ProjectNotFound("x".into()),
            Error::AgentNotFound("x".into()),
            Error::MessageNotFound(1),
            Error::ThreadNotFound("x".into()),
            Error::ReservationNotFound(1),
            Error::ProductNotFound("x".into()),
            Error::InvalidArgument("x".into()),
            Error::InvalidAgentName("x".into()),
            Error::InvalidThreadId("x".into()),
            Error::InvalidProjectKey("x".into()),
            Error::MissingField("x".into()),
            Error::TypeError("x".into()),
            Error::ContactRequired {
                from: "a".into(),
                to: "b".into(),
            },
            Error::ContactBlocked {
                from: "a".into(),
                to: "b".into(),
            },
            Error::Database("x".into()),
            Error::DatabasePoolExhausted,
            Error::DatabaseLockTimeout,
            Error::GitIndexLock,
            Error::ArchiveLockTimeout("x".into()),
            Error::ReservationConflict {
                pattern: "x".into(),
                holders: vec![],
            },
            Error::ResourceBusy("x".into()),
            Error::ResourceExhausted("x".into()),
            Error::Timeout("x".into()),
            Error::Cancelled,
            Error::Connection("x".into()),
        ];
        for err in &recoverable {
            assert!(err.is_recoverable(), "Error {err:?} should be recoverable");
        }

        // Non-recoverable errors (false)
        let non_recoverable = vec![
            Error::CapabilityDenied("x".into()),
            Error::PermissionDenied("x".into()),
            Error::Git("x".into()),
            Error::Internal("x".into()),
            Error::Io(std::io::Error::other("x")),
        ];
        for err in &non_recoverable {
            assert!(
                !err.is_recoverable(),
                "Error {err:?} should NOT be recoverable"
            );
        }
    }

    /// Verify all error types from the legacy Python codebase are represented.
    #[test]
    fn test_all_legacy_error_codes_present() {
        let expected_codes = [
            "NOT_FOUND",
            "INVALID_ARGUMENT",
            "MISSING_FIELD",
            "TYPE_ERROR",
            "CONTACT_REQUIRED",
            "CONTACT_BLOCKED",
            "CAPABILITY_DENIED",
            "PERMISSION_ERROR",
            "RESOURCE_BUSY",
            "RESOURCE_EXHAUSTED",
            "DATABASE_ERROR",
            "DATABASE_POOL_EXHAUSTED",
            "GIT_INDEX_LOCK",
            "ARCHIVE_LOCK_TIMEOUT",
            "UNHANDLED_EXCEPTION",
            "TIMEOUT",
            "OS_ERROR",
            "CONNECTION_ERROR",
        ];

        // Collect all error_type strings produced by our variants
        let produced: Vec<&str> = vec![
            Error::ProjectNotFound(String::new()).error_type(),
            Error::InvalidArgument(String::new()).error_type(),
            Error::MissingField(String::new()).error_type(),
            Error::TypeError(String::new()).error_type(),
            Error::ContactRequired {
                from: String::new(),
                to: String::new(),
            }
            .error_type(),
            Error::ContactBlocked {
                from: String::new(),
                to: String::new(),
            }
            .error_type(),
            Error::CapabilityDenied(String::new()).error_type(),
            Error::PermissionDenied(String::new()).error_type(),
            Error::ResourceBusy(String::new()).error_type(),
            Error::ResourceExhausted(String::new()).error_type(),
            Error::Database(String::new()).error_type(),
            Error::DatabasePoolExhausted.error_type(),
            Error::GitIndexLock.error_type(),
            Error::ArchiveLockTimeout(String::new()).error_type(),
            Error::Git(String::new()).error_type(),
            Error::Timeout(String::new()).error_type(),
            Error::Io(std::io::Error::other("")).error_type(),
            Error::Connection(String::new()).error_type(),
        ];

        for code in &expected_codes {
            assert!(
                produced.contains(code),
                "Legacy error code '{code}' is not produced by any Error variant"
            );
        }
    }
}
