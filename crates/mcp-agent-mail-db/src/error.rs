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

/// Coarse, stable class for database and database-adjacent operational errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorClass {
    /// Authoritative main database B-tree/header/page corruption.
    MainDbBtreeCorruption,
    /// WAL/SHM sidecar corruption or truncation, distinct from main DB damage.
    WalSidecarCorruption,
    /// Schema drift, missing tables/columns, or migration shape mismatch.
    SchemaDriftOrMissingTables,
    /// The engine/probe failed in a way that is not enough to diagnose DB damage.
    EngineProbeLimitation,
    /// Foreign-key graph inconsistency or orphaned rows.
    ForeignKeyInconsistency,
    /// FTS/search index corruption that can be rebuilt from canonical rows.
    FtsIndexCorruption,
    /// Connection, path, permission, or configuration error.
    ConnectionOrConfigError,
    /// Retryable busy/lock/MVCC contention.
    BusyRetryable,
    /// Process file-descriptor exhaustion.
    FdExhaustion,
    /// Connection pool exhaustion.
    PoolExhaustion,
    /// A live mailbox owner refused a direct write.
    LiveOwnerNoActivityLock,
    /// Host resource pressure such as disk-full or read-only filesystem.
    HostPressure,
}

impl DbErrorClass {
    /// Stable machine string for JSON/error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainDbBtreeCorruption => "main_db_btree_corruption",
            Self::WalSidecarCorruption => "wal_sidecar_corruption",
            Self::SchemaDriftOrMissingTables => "schema_drift_or_missing_tables",
            Self::EngineProbeLimitation => "engine_probe_limitation",
            Self::ForeignKeyInconsistency => "foreign_key_inconsistency",
            Self::FtsIndexCorruption => "fts_index_corruption",
            Self::ConnectionOrConfigError => "connection_or_config_error",
            Self::BusyRetryable => "busy_retryable",
            Self::FdExhaustion => "fd_exhaustion",
            Self::PoolExhaustion => "pool_exhaustion",
            Self::LiveOwnerNoActivityLock => "live_owner_no_activity_lock",
            Self::HostPressure => "host_pressure",
        }
    }
}

/// Severity attached to a [`DbErrorClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorSeverity {
    /// Data-loss or edit-blocking condition.
    P0,
    /// Serious operational outage or direct-write safety issue.
    P1,
    /// Degraded but bounded condition.
    P2,
    /// Local configuration/operator issue.
    P3,
}

impl DbErrorSeverity {
    /// Stable machine string for JSON/error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
        }
    }
}

/// Policy metadata for a classified database failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbErrorClassification {
    pub class: DbErrorClass,
    pub severity: DbErrorSeverity,
    pub repairable: bool,
    pub safe_to_retry: bool,
    pub safe_to_continue_read_only: bool,
    pub blocks_edits: bool,
    pub recommended_command: &'static str,
}

impl DbErrorClassification {
    #[must_use]
    pub const fn for_class(class: DbErrorClass) -> Self {
        match class {
            DbErrorClass::MainDbBtreeCorruption => Self {
                class,
                severity: DbErrorSeverity::P0,
                repairable: true,
                safe_to_retry: false,
                safe_to_continue_read_only: false,
                blocks_edits: true,
                recommended_command: "am doctor --json",
            },
            DbErrorClass::WalSidecarCorruption => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: true,
                safe_to_retry: false,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor health",
            },
            DbErrorClass::SchemaDriftOrMissingTables => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: true,
                safe_to_retry: false,
                safe_to_continue_read_only: false,
                blocks_edits: true,
                recommended_command: "am doctor migrate --check",
            },
            DbErrorClass::EngineProbeLimitation => Self {
                class,
                severity: DbErrorSeverity::P2,
                repairable: false,
                safe_to_retry: false,
                safe_to_continue_read_only: true,
                blocks_edits: false,
                recommended_command: "am doctor health",
            },
            DbErrorClass::ForeignKeyInconsistency => Self {
                class,
                severity: DbErrorSeverity::P0,
                repairable: true,
                safe_to_retry: false,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor --json",
            },
            DbErrorClass::FtsIndexCorruption => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: true,
                safe_to_retry: false,
                safe_to_continue_read_only: true,
                blocks_edits: false,
                recommended_command: "am doctor fix --list --json",
            },
            DbErrorClass::ConnectionOrConfigError => Self {
                class,
                severity: DbErrorSeverity::P3,
                repairable: false,
                safe_to_retry: false,
                safe_to_continue_read_only: false,
                blocks_edits: true,
                recommended_command: "am doctor health",
            },
            DbErrorClass::BusyRetryable => Self {
                class,
                severity: DbErrorSeverity::P2,
                repairable: false,
                safe_to_retry: true,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor locks",
            },
            DbErrorClass::FdExhaustion => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: false,
                safe_to_retry: true,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor health",
            },
            DbErrorClass::PoolExhaustion => Self {
                class,
                severity: DbErrorSeverity::P2,
                repairable: false,
                safe_to_retry: true,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor health",
            },
            DbErrorClass::LiveOwnerNoActivityLock => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: false,
                safe_to_retry: true,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "route the write through the running Agent Mail server",
            },
            DbErrorClass::HostPressure => Self {
                class,
                severity: DbErrorSeverity::P1,
                repairable: false,
                safe_to_retry: true,
                safe_to_continue_read_only: true,
                blocks_edits: true,
                recommended_command: "am doctor health",
            },
        }
    }
}

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
            Self::Sqlite(msg) | Self::Pool(msg) | Self::Schema(msg) | Self::Internal(msg) => {
                classify_db_error_message(msg).safe_to_retry
            }
            Self::ResourceBusy(_) | Self::PoolExhausted { .. } => true,
            _ => false,
        }
    }

    /// Whether this error indicates database corruption that may be
    /// recoverable via backup restore or archive reconstruction.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Sqlite(msg) | Self::Pool(msg) | Self::Schema(msg) => is_corruption_error(msg),
            Self::IntegrityCorruption { .. } => true,
            _ => false,
        }
    }

    /// Typed classification and policy metadata for this database error.
    #[must_use]
    pub fn classification(&self) -> DbErrorClassification {
        match self {
            Self::PoolExhausted { .. } => {
                DbErrorClassification::for_class(DbErrorClass::PoolExhaustion)
            }
            Self::ResourceBusy(message) | Self::CircuitBreakerOpen { message, .. } => {
                let class = if is_mailbox_ownership_contention(message) {
                    DbErrorClass::LiveOwnerNoActivityLock
                } else {
                    DbErrorClass::BusyRetryable
                };
                DbErrorClassification::for_class(class)
            }
            Self::IntegrityCorruption { message, details } => {
                let joined_details = details.join("\n");
                let class = if contains_fts_index_corruption(message)
                    || contains_fts_index_corruption(&joined_details)
                {
                    DbErrorClass::FtsIndexCorruption
                } else if contains_foreign_key_inconsistency(message)
                    || contains_foreign_key_inconsistency(&joined_details)
                {
                    DbErrorClass::ForeignKeyInconsistency
                } else {
                    DbErrorClass::MainDbBtreeCorruption
                };
                DbErrorClassification::for_class(class)
            }
            Self::Pool(message) => {
                let classification = classify_db_error_message(message);
                if classification.class == DbErrorClass::ConnectionOrConfigError
                    && message.trim().eq_ignore_ascii_case("timeout")
                {
                    DbErrorClassification::for_class(DbErrorClass::PoolExhaustion)
                } else {
                    classification
                }
            }
            Self::Sqlite(message) | Self::Schema(message) => classify_db_error_message(message),
            Self::Internal(message) => classify_db_error_message(message),
            Self::NotFound { .. }
            | Self::Duplicate { .. }
            | Self::InvalidArgument { .. }
            | Self::Serialization(_) => {
                DbErrorClassification::for_class(DbErrorClass::ConnectionOrConfigError)
            }
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
    matches!(
        classify_db_error_message(msg).class,
        DbErrorClass::BusyRetryable | DbErrorClass::LiveOwnerNoActivityLock
    )
}

/// Check whether a direct mutation was refused because a live mailbox owner is
/// already active.
///
/// The live owner is typically a long-running `am serve-http` daemon holding
/// the mailbox's activity lock.
///
/// This is distinct from a raw `SQLITE_BUSY` ("database is locked"): the
/// mailbox ownership gate (`refuse_mutating_mailbox_when_owned` /
/// `evaluate_write_route`) rejects the write *before* it ever touches the
/// SQLite file, emitting a `SqlError::Custom` that does not contain the
/// classic lock phrasing. Without this classifier those refusals fall through
/// to the generic "A database error occurred" mapping in the tools layer
/// (issue #139) instead of surfacing as a retryable `RESOURCE_BUSY` with an
/// actionable "route through the running server" hint.
#[must_use]
pub fn is_mailbox_ownership_contention(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("mailbox mutation refused")
        || lower.contains("owns this mailbox")
        || lower.contains("owns the mailbox")
        || lower.contains("already owns the mailbox")
        || lower.contains("hold the mailbox lock")
        || lower.contains("holds the mailbox lock")
        || lower.contains("competing processes hold locks")
        || lower.contains("mailbox activity lock")
        || lower.contains("route writes through that process")
        || lower.contains("wait for the active owner")
}

/// Check whether an error message indicates an MVCC write conflict
/// (frankensqlite `BEGIN CONCURRENT` page-level collision).
#[must_use]
pub fn is_mvcc_conflict(msg: &str) -> bool {
    contains_mvcc_conflict(msg)
}

/// Check whether an error message indicates database corruption
/// (malformed image, corrupt schema, etc.) that may be recoverable
/// via backup restore or archive reconstruction.
#[must_use]
pub fn is_corruption_error(msg: &str) -> bool {
    matches!(
        classify_db_error_message(msg).class,
        DbErrorClass::MainDbBtreeCorruption
    )
}

/// Check whether an error message indicates pool exhaustion.
#[must_use]
pub fn is_pool_exhausted_error(msg: &str) -> bool {
    classify_db_error_message(msg).class == DbErrorClass::PoolExhaustion
}

/// Check whether an error message indicates process file-descriptor exhaustion.
#[must_use]
pub fn is_fd_exhaustion_error(msg: &str) -> bool {
    classify_db_error_message(msg).class == DbErrorClass::FdExhaustion
}

/// Classify a raw database/IO/probe error message into a typed failure class.
#[must_use]
pub fn classify_db_error_message(msg: &str) -> DbErrorClassification {
    let class = classify_db_error_message_class(msg);
    DbErrorClassification::for_class(class)
}

fn classify_db_error_message_class(msg: &str) -> DbErrorClass {
    if is_mailbox_ownership_contention(msg) {
        return DbErrorClass::LiveOwnerNoActivityLock;
    }
    if contains_fd_exhaustion(msg) {
        return DbErrorClass::FdExhaustion;
    }
    if contains_pool_exhaustion(msg) {
        return DbErrorClass::PoolExhaustion;
    }
    if contains_host_pressure(msg) {
        return DbErrorClass::HostPressure;
    }
    if contains_mvcc_conflict(msg) || contains_lock_or_busy(msg) {
        return DbErrorClass::BusyRetryable;
    }
    if contains_wal_sidecar_corruption(msg) {
        return DbErrorClass::WalSidecarCorruption;
    }
    if contains_fts_index_corruption(msg) {
        return DbErrorClass::FtsIndexCorruption;
    }
    if contains_foreign_key_inconsistency(msg) {
        return DbErrorClass::ForeignKeyInconsistency;
    }
    if contains_schema_drift(msg) {
        return DbErrorClass::SchemaDriftOrMissingTables;
    }
    if contains_main_db_corruption(msg) {
        return DbErrorClass::MainDbBtreeCorruption;
    }
    if contains_engine_probe_limitation(msg) {
        return DbErrorClass::EngineProbeLimitation;
    }
    DbErrorClass::ConnectionOrConfigError
}

fn contains_lock_or_busy(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("database is locked")
        || lower.contains("database table is locked")
        || lower.contains("database schema is locked")
        || lower.contains("database is busy")
        || lower.contains("resource temporarily busy")
        || lower.contains("sqlite_busy")
        || lower.contains("locked by another process")
        || lower.contains("unable to open database")
        || lower.contains("disk i/o error")
}

fn contains_mvcc_conflict(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("write conflict on page")
        || lower.contains("snapshot conflict on pages")
        || lower.contains("serialization failure")
        || lower.contains("busy_snapshot")
        || lower.contains("snapshot too old")
}

fn contains_main_db_corruption(msg: &str) -> bool {
    let lower = msg.to_lowercase();
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
}

fn contains_wal_sidecar_corruption(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    (lower.contains("wal")
        || lower.contains("-wal")
        || lower.contains("shm")
        || lower.contains("-shm"))
        && (lower.contains("too small")
            || lower.contains("malformed")
            || lower.contains("invalid")
            || lower.contains("checksum")
            || lower.contains("sidecar")
            || lower.contains("header"))
}

fn contains_schema_drift(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("no such table")
        || lower.contains("no such column")
        || lower.contains("missing table")
        || lower.contains("missing column")
        || lower.contains("schema version mismatch")
        || lower.contains("schema drift")
}

fn contains_engine_probe_limitation(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("out of memory")
        || lower.contains("cursor stack is empty")
        || lower.contains("called `option::unwrap()` on a `none` value")
        || lower.contains("cursor must be on a leaf")
        || (lower.contains("internal error") && !contains_main_db_corruption(msg))
}

fn contains_foreign_key_inconsistency(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("foreign key check")
        || lower.contains("foreign_key_check")
        || lower.contains("foreign key mismatch")
        || lower.contains("foreign key inconsistency")
        || lower.contains("orphan foreign key")
        || lower.contains("orphaned foreign key")
        || lower.contains("orphaned recipient")
}

fn contains_fts_index_corruption(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    (lower.contains("fts") || lower.contains("search index"))
        && (lower.contains("corrupt")
            || lower.contains("malformed")
            || lower.contains("integrity")
            || lower.contains("checksum")
            || lower.contains("missing"))
}

fn contains_pool_exhaustion(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    let lower = lower.trim();
    (lower.contains("pool") && (lower.contains("timeout") || lower.contains("exhausted")))
        || lower.contains("queuepool")
        || lower == "timeout"
        || lower.contains("timed out waiting for connection")
        || lower.contains("timeout waiting for connection")
}

fn contains_fd_exhaustion(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    let canonical_open_file_limit = lower.contains("too many open files")
        || lower.contains("emfile")
        || lower.contains("os error 24");
    let descriptor_limit = lower.contains("file descriptor")
        && (lower.contains("limit")
            || lower.contains("exhaust")
            || lower.contains("out of")
            || lower.contains("too many")
            || lower.contains("table full"));
    let explicit_open_file_limit = lower.contains("open file limit")
        || lower.contains("open-file limit")
        || lower.contains("open files limit");
    let open_file_limit = explicit_open_file_limit
        || (lower.contains("open files")
            && (lower.contains("limit")
                || lower.contains("exhaust")
                || lower.contains("maximum")
                || lower.contains("exceeded")
                || lower.contains("out of")));
    canonical_open_file_limit || descriptor_limit || open_file_limit
}

fn contains_host_pressure(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("no space left on device")
        || lower.contains("disk full")
        || lower.contains("database or disk is full")
        || lower.contains("readonly database")
        || lower.contains("read-only file system")
        || lower.contains("input/output error")
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

    #[test]
    fn fd_exhaustion_is_retryable_host_pressure_not_corruption() {
        let message = "send_message failed: Too many open files. Freed 0 cached repos";
        assert!(is_fd_exhaustion_error(message));
        assert!(!is_corruption_error(message));
        assert!(DbError::Sqlite(message.to_string()).is_retryable());
        assert!(DbError::Pool(message.to_string()).is_retryable());
        assert!(DbError::Schema(message.to_string()).is_retryable());
        assert!(DbError::Internal(message.to_string()).is_retryable());
    }

    #[test]
    fn fd_exhaustion_detector_requires_limit_or_exhaustion_signal() {
        assert!(is_fd_exhaustion_error(
            "open failed: Too many open files (os error 24)"
        ));
        assert!(is_fd_exhaustion_error(
            "EMFILE while opening storage database"
        ));
        assert!(is_fd_exhaustion_error(
            "file descriptor limit exhausted for process"
        ));
        assert!(is_fd_exhaustion_error("open-file limit reached"));
        assert!(is_fd_exhaustion_error("out of file descriptors"));
        assert!(!is_fd_exhaustion_error("bad file descriptor"));
        assert!(!is_fd_exhaustion_error("invalid file descriptor"));
        assert!(!is_fd_exhaustion_error(
            "poll failed because the file descriptor was closed"
        ));
        assert!(!is_fd_exhaustion_error("Freed 0 cached repos"));
        assert!(!DbError::Internal("bad file descriptor".into()).is_retryable());
    }

    fn assert_class(message: &str, expected: DbErrorClass) -> DbErrorClassification {
        let classification = classify_db_error_message(message);
        assert_eq!(
            classification.class, expected,
            "unexpected classification for {message:?}: {classification:?}"
        );
        classification
    }

    #[test]
    fn typed_classifier_covers_historical_a1_error_families() {
        assert_class(
            "database disk image is malformed",
            DbErrorClass::MainDbBtreeCorruption,
        );
        assert_class(
            "WAL file too small for header during rebuild: read 0, need 32",
            DbErrorClass::WalSidecarCorruption,
        );
        assert_class(
            "no such column: messages.thread_id",
            DbErrorClass::SchemaDriftOrMissingTables,
        );
        assert_class(
            "frankensqlite internal error: cursor stack is empty",
            DbErrorClass::EngineProbeLimitation,
        );
        assert_class(
            "PRAGMA foreign_key_check reported orphaned recipient rows",
            DbErrorClass::ForeignKeyInconsistency,
        );
        assert_class(
            "fts5 search index integrity check failed",
            DbErrorClass::FtsIndexCorruption,
        );
        assert!(!is_corruption_error(
            "fts5 search index integrity check failed"
        ));
        assert_class("unable to open database file", DbErrorClass::BusyRetryable);
        assert_class("database is locked", DbErrorClass::BusyRetryable);
        assert_class(
            "Too many open files (os error 24)",
            DbErrorClass::FdExhaustion,
        );
        assert_class("QueuePool limit reached", DbErrorClass::PoolExhaustion);
        assert_class(
            "mailbox mutation refused: another Agent Mail server owns this mailbox",
            DbErrorClass::LiveOwnerNoActivityLock,
        );
        assert_class("database or disk is full", DbErrorClass::HostPressure);
        assert_class("connection refused", DbErrorClass::ConnectionOrConfigError);
    }

    #[test]
    fn typed_classifier_policy_metadata_matches_retry_and_edit_safety() {
        let busy = assert_class("snapshot conflict on pages: 7", DbErrorClass::BusyRetryable);
        assert!(busy.safe_to_retry);
        assert!(busy.safe_to_continue_read_only);
        assert!(busy.blocks_edits);
        assert_eq!(busy.recommended_command, "am doctor locks");

        let corruption = assert_class(
            "page 12: xxh3 page checksum mismatch",
            DbErrorClass::MainDbBtreeCorruption,
        );
        assert_eq!(corruption.severity, DbErrorSeverity::P0);
        assert!(corruption.repairable);
        assert!(!corruption.safe_to_retry);
        assert!(!corruption.safe_to_continue_read_only);
        assert!(corruption.blocks_edits);

        let schema = assert_class(
            "no such table: inbox_stats",
            DbErrorClass::SchemaDriftOrMissingTables,
        );
        assert!(!schema.safe_to_retry);
        assert!(schema.blocks_edits);
        assert_eq!(schema.recommended_command, "am doctor migrate --check");
    }

    #[test]
    fn db_error_variant_classification_overrides_raw_detail_when_needed() {
        let integrity = DbError::IntegrityCorruption {
            message: "integrity failed".into(),
            details: vec!["fts5 search index malformed".into()],
        };
        assert_eq!(
            integrity.classification().class,
            DbErrorClass::FtsIndexCorruption
        );

        let pool = DbError::PoolExhausted {
            message: "all connections in use".into(),
            pool_size: 4,
            max_overflow: 2,
        };
        assert_eq!(pool.classification().class, DbErrorClass::PoolExhaustion);

        let plain_pool_timeout = DbError::Pool("timeout".into());
        assert_eq!(
            plain_pool_timeout.classification().class,
            DbErrorClass::PoolExhaustion
        );

        let owner =
            DbError::ResourceBusy("route writes through that process; it owns this mailbox".into());
        assert_eq!(
            owner.classification().class,
            DbErrorClass::LiveOwnerNoActivityLock
        );
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

    // ── is_mailbox_ownership_contention (#139) ──────────────────────────

    #[test]
    fn mailbox_ownership_contention_patterns() {
        // The exact phrasing emitted by `refuse_mutating_mailbox_when_owned`
        // (pool.rs) when a running server owns the mailbox.
        assert!(is_mailbox_ownership_contention(
            "mailbox mutation refused for /tmp/mb/storage.sqlite3: \
             another Agent Mail server owns the mailbox database: pid 4242; \
             wait for the active owner to finish instead of competing recovery"
        ));
        // The write-route refusal reasons from `evaluate_write_route`.
        assert!(is_mailbox_ownership_contention(
            "Another active process owns this mailbox (pid 17). \
             Route writes through that process or stop it first."
        ));
        assert!(is_mailbox_ownership_contention(
            "A stale process appears to hold the mailbox lock."
        ));
        assert!(is_mailbox_ownership_contention(
            "live Agent Mail process still holds the mailbox database \
             without mailbox activity locks: pid 9"
        ));
        // Case-insensitive.
        assert!(is_mailbox_ownership_contention("MAILBOX MUTATION REFUSED"));
    }

    #[test]
    fn mailbox_ownership_contention_is_lock_error() {
        // #139: ownership-contention refusals must classify as a lock/busy
        // condition so the tools layer maps them to RESOURCE_BUSY (retryable)
        // rather than the generic, non-actionable "A database error occurred".
        let refusal = "mailbox mutation refused for /tmp/mb/storage.sqlite3: \
             another Agent Mail server owns the mailbox database: pid 4242; \
             wait for the active owner to finish instead of competing recovery";
        assert!(is_lock_error(refusal));
        let e = DbError::Sqlite(refusal.to_string());
        assert!(e.is_retryable());
        assert_eq!(e.error_code(), "INTERNAL_ERROR"); // bare DbError code unchanged
    }

    #[test]
    fn not_mailbox_ownership_contention() {
        assert!(!is_mailbox_ownership_contention("syntax error near SELECT"));
        assert!(!is_mailbox_ownership_contention("database is locked"));
        assert!(!is_mailbox_ownership_contention(
            "no competing Agent Mail mailbox owners or live database holders detected"
        ));
        assert!(!is_mailbox_ownership_contention(""));
    }

    // ── is_mvcc_conflict ────────────────────────────────────────────

    #[test]
    fn mvcc_conflict_patterns() {
        assert!(is_mvcc_conflict(
            "write conflict on page 42: held by transaction 7"
        ));
        assert!(is_mvcc_conflict(
            "database is busy (snapshot conflict on pages: 42)"
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

    // ── is_corruption ────────────────────────────────────────────────

    #[test]
    fn corruption_error_from_sqlite_message() {
        let e = DbError::Sqlite("database disk image is malformed".into());
        assert!(e.is_corruption());
    }

    #[test]
    fn corruption_error_from_pool_message() {
        let e = DbError::Pool("database disk image is malformed".into());
        assert!(e.is_corruption());
    }

    #[test]
    fn corruption_error_from_schema_message() {
        let e = DbError::Schema("database disk image is malformed".into());
        assert!(e.is_corruption());
    }

    #[test]
    fn corruption_error_from_integrity_variant() {
        let e = DbError::IntegrityCorruption {
            message: "bad page".into(),
            details: vec!["page 42".into()],
        };
        assert!(e.is_corruption());
    }

    #[test]
    fn not_corruption_for_lock_error() {
        let e = DbError::Sqlite("database is locked".into());
        assert!(!e.is_corruption());
    }

    #[test]
    fn not_corruption_for_syntax_error() {
        let e = DbError::Sqlite("syntax error near SELECT".into());
        assert!(!e.is_corruption());
    }

    #[test]
    fn not_corruption_for_schema_migration_failure() {
        let e = DbError::Schema("migration v4 failed".into());
        assert!(!e.is_corruption());
    }

    // ── is_corruption_error (function) ─────────────────────────────

    #[test]
    fn corruption_error_patterns() {
        assert!(is_corruption_error("database disk image is malformed"));
        assert!(is_corruption_error("Database Disk Image Is Malformed")); // case-insensitive
        assert!(is_corruption_error("malformed database schema: agents"));
        assert!(is_corruption_error("database schema is corrupt"));
        assert!(is_corruption_error("file is not a database"));
        assert!(is_corruption_error(
            "database file too small for header: 14 bytes (< 100)"
        ));
        assert!(is_corruption_error("invalid database header: bad magic"));
        assert!(is_corruption_error("page 12: xxh3 page checksum mismatch"));
        assert!(is_corruption_error("malformed page 42 in btree"));
    }

    #[test]
    fn not_corruption_error() {
        assert!(!is_corruption_error("database is locked"));
        assert!(!is_corruption_error("syntax error"));
        assert!(!is_corruption_error("table not found"));
        assert!(!is_corruption_error(""));
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
