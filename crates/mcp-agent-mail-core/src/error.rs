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
            Self::ArchiveLockTimeout(_) | Self::Timeout(_) | Self::Cancelled => "TIMEOUT",
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

    #[test]
    fn test_error_types() {
        assert_eq!(
            Error::ProjectNotFound("test".into()).error_type(),
            "NOT_FOUND"
        );
        assert_eq!(
            Error::InvalidArgument("test".into()).error_type(),
            "INVALID_ARGUMENT"
        );
        assert_eq!(
            Error::ContactRequired {
                from: "a".into(),
                to: "b".into()
            }
            .error_type(),
            "CONTACT_REQUIRED"
        );
    }

    #[test]
    fn test_recoverable() {
        assert!(Error::DatabaseLockTimeout.is_recoverable());
        assert!(Error::Timeout("test".into()).is_recoverable());
        assert!(Error::ProjectNotFound("test".into()).is_recoverable());
        assert!(!Error::CapabilityDenied("no".into()).is_recoverable());
        assert!(!Error::PermissionDenied("no".into()).is_recoverable());
    }
}
