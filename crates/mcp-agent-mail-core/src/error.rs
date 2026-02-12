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

    // =========================================================================
    // br-3h13.1.2: Display impl tests for every error variant
    // =========================================================================

    #[test]
    fn test_display_not_found_variants() {
        assert_eq!(
            Error::ProjectNotFound("proj-a".into()).to_string(),
            "Project not found: proj-a"
        );
        assert_eq!(
            Error::AgentNotFound("BlueLake".into()).to_string(),
            "Agent not found: BlueLake"
        );
        assert_eq!(
            Error::MessageNotFound(42).to_string(),
            "Message not found: 42"
        );
        assert_eq!(
            Error::ThreadNotFound("TKT-1".into()).to_string(),
            "Thread not found: TKT-1"
        );
        assert_eq!(
            Error::ReservationNotFound(7).to_string(),
            "File reservation not found: 7"
        );
        assert_eq!(
            Error::ProductNotFound("p1".into()).to_string(),
            "Product not found: p1"
        );
    }

    #[test]
    fn test_display_validation_variants() {
        assert_eq!(
            Error::InvalidArgument("bad arg".into()).to_string(),
            "Invalid argument: bad arg"
        );
        assert_eq!(
            Error::InvalidAgentName("Xyz".into()).to_string(),
            "Invalid agent name: Xyz. Must be adjective+noun format (e.g., GreenLake)"
        );
        assert_eq!(
            Error::InvalidThreadId("../evil".into()).to_string(),
            "Invalid thread ID: ../evil. Must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"
        );
        assert_eq!(
            Error::InvalidProjectKey("relative".into()).to_string(),
            "Invalid project key: relative. Must be absolute path"
        );
        assert_eq!(
            Error::MissingField("sender_name".into()).to_string(),
            "Missing required field: sender_name"
        );
        assert_eq!(
            Error::TypeError("expected int".into()).to_string(),
            "Type error: expected int"
        );
    }

    #[test]
    fn test_display_contact_auth_variants() {
        assert_eq!(
            Error::ContactRequired {
                from: "A".into(),
                to: "B".into(),
            }
            .to_string(),
            "Contact required: A -> B"
        );
        assert_eq!(
            Error::ContactBlocked {
                from: "A".into(),
                to: "B".into(),
            }
            .to_string(),
            "Contact blocked: A -> B"
        );
        assert_eq!(
            Error::CapabilityDenied("admin only".into()).to_string(),
            "Capability denied: admin only"
        );
        assert_eq!(
            Error::PermissionDenied("read-only".into()).to_string(),
            "Permission denied: read-only"
        );
    }

    #[test]
    fn test_display_conflict_variants() {
        assert_eq!(
            Error::ReservationConflict {
                pattern: "src/*.rs".into(),
                holders: vec!["BlueLake".into(), "RedFox".into()],
            }
            .to_string(),
            "File reservation conflict on pattern 'src/*.rs'. Held by: [\"BlueLake\", \"RedFox\"]"
        );
        assert_eq!(
            Error::ResourceBusy("slot-1".into()).to_string(),
            "Resource busy: slot-1"
        );
        assert_eq!(
            Error::ResourceExhausted("connections".into()).to_string(),
            "Resource exhausted: connections"
        );
    }

    #[test]
    fn test_display_database_variants() {
        assert_eq!(
            Error::Database("table locked".into()).to_string(),
            "Database error: table locked"
        );
        assert_eq!(
            Error::DatabasePoolExhausted.to_string(),
            "Database pool exhausted"
        );
        assert_eq!(
            Error::DatabaseLockTimeout.to_string(),
            "Database lock timeout"
        );
    }

    #[test]
    fn test_display_git_archive_variants() {
        assert_eq!(
            Error::Git("merge conflict".into()).to_string(),
            "Git error: merge conflict"
        );
        assert_eq!(
            Error::GitIndexLock.to_string(),
            "Git index lock held by another process"
        );
        assert_eq!(
            Error::ArchiveLockTimeout("proj-x".into()).to_string(),
            "Archive lock timeout for project: proj-x"
        );
    }

    #[test]
    fn test_display_io_variants() {
        let io_err = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(io_err.to_string().contains("gone"));

        let json_err: serde_json::Error = serde_json::from_str::<i32>("nope").unwrap_err();
        let ser_err = Error::Serialization(json_err);
        assert!(ser_err.to_string().starts_with("Serialization error:"));
    }

    #[test]
    fn test_display_timeout_variants() {
        assert_eq!(
            Error::Timeout("5s elapsed".into()).to_string(),
            "Operation timed out: 5s elapsed"
        );
        assert_eq!(Error::Cancelled.to_string(), "Operation cancelled");
    }

    #[test]
    fn test_display_connection_internal_variants() {
        assert_eq!(
            Error::Connection("refused".into()).to_string(),
            "Connection error: refused"
        );
        assert_eq!(
            Error::Internal("unexpected state".into()).to_string(),
            "Internal error: unexpected state"
        );
    }

    /// Verify that every error variant's Display output is non-empty.
    #[test]
    fn test_display_all_non_empty() {
        let all_errors: Vec<Error> = vec![
            Error::ProjectNotFound(String::new()),
            Error::AgentNotFound(String::new()),
            Error::MessageNotFound(0),
            Error::ThreadNotFound(String::new()),
            Error::ReservationNotFound(0),
            Error::ProductNotFound(String::new()),
            Error::InvalidArgument(String::new()),
            Error::InvalidAgentName(String::new()),
            Error::InvalidThreadId(String::new()),
            Error::InvalidProjectKey(String::new()),
            Error::MissingField(String::new()),
            Error::TypeError(String::new()),
            Error::ContactRequired {
                from: String::new(),
                to: String::new(),
            },
            Error::ContactBlocked {
                from: String::new(),
                to: String::new(),
            },
            Error::CapabilityDenied(String::new()),
            Error::PermissionDenied(String::new()),
            Error::ReservationConflict {
                pattern: String::new(),
                holders: vec![],
            },
            Error::ResourceBusy(String::new()),
            Error::ResourceExhausted(String::new()),
            Error::Database(String::new()),
            Error::DatabasePoolExhausted,
            Error::DatabaseLockTimeout,
            Error::Git(String::new()),
            Error::GitIndexLock,
            Error::ArchiveLockTimeout(String::new()),
            Error::Io(std::io::Error::other("")),
            Error::Serialization(serde_json::from_str::<i32>("x").unwrap_err()),
            Error::Timeout(String::new()),
            Error::Cancelled,
            Error::Connection(String::new()),
            Error::Internal(String::new()),
        ];
        for err in &all_errors {
            let display = err.to_string();
            assert!(
                !display.is_empty(),
                "Error {err:?} should have non-empty Display"
            );
        }
    }
}
