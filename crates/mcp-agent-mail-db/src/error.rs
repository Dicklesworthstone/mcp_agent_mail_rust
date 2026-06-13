//! Error types for the database layer

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
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

    /// A bounded retry loop spent its entire budget without success (D3).
    ///
    /// Wraps the final error so downstream consumers can render an honest,
    /// budget-aware envelope ("the server already retried N times over X ms")
    /// instead of advising another blind retry. Classification, retryability,
    /// and legacy error codes all delegate to the wrapped error.
    #[error(
        "{operation}: retry budget exhausted ({attempts}/{budget} attempts, {elapsed_ms} ms elapsed): {inner}"
    )]
    RetryBudgetExhausted {
        /// Logical operation name (e.g. `create_message`).
        operation: &'static str,
        /// Attempts actually made (initial attempt + retries).
        attempts: u32,
        /// Total attempt budget that was available.
        budget: u32,
        /// Wall-clock time spent inside the retry loop, in milliseconds.
        elapsed_ms: u64,
        /// The final error that exhausted the budget.
        inner: Box<Self>,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "diagnostic policy is intentionally a flat JSON-facing set of independent facts"
)]
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

pub const DB_FAILURE_ENVELOPE_SCHEMA_VERSION: &str = "am.db_failure_envelope.v1";

/// Stable robot-facing policy section for a classified database failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "diagnostic policy is intentionally a flat JSON-facing set of independent facts"
)]
pub struct DbFailurePolicy {
    pub repairable: bool,
    pub safe_to_retry: bool,
    pub safe_to_continue_read_only: bool,
    pub blocks_edits: bool,
    pub recommended_command: &'static str,
}

impl From<DbErrorClassification> for DbFailurePolicy {
    fn from(classification: DbErrorClassification) -> Self {
        Self {
            repairable: classification.repairable,
            safe_to_retry: classification.safe_to_retry,
            safe_to_continue_read_only: classification.safe_to_continue_read_only,
            blocks_edits: classification.blocks_edits,
            recommended_command: classification.recommended_command,
        }
    }
}

/// Process identity captured without opening or mutating the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureProcessIdentity {
    pub pid: u32,
    pub binary_path: Option<String>,
    pub binary_version: &'static str,
}

/// Best-effort sidecar state for a SQLite WAL/SHM file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureSidecarState {
    pub path: Option<String>,
    pub exists: bool,
    pub len_bytes: Option<u64>,
    pub modified_unix_ms: Option<u64>,
    pub modified_age_ms: Option<u64>,
    pub inspect_error: Option<String>,
}

/// Best-effort WAL/SHM state captured from filesystem metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureSidecars {
    pub wal: DbFailureSidecarState,
    pub shm: DbFailureSidecarState,
}

/// Probe status that is safe to include from an error-path formatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureProbeResult {
    pub status: &'static str,
    pub detail: Option<String>,
}

/// Host context collected with side-effect-free std/procfs reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureHostSummary {
    pub loadavg: Option<String>,
    pub disk_summary: Option<String>,
    pub inspect_error: Option<String>,
}

/// Bounded-retry report attached when a retry loop exhausted its budget (D3).
///
/// `immediate_retry_useful` is the instance-level honesty bit: the static
/// class policy (`DbFailurePolicy::safe_to_retry`) says whether this error
/// *family* is ever retryable, while this field says whether retrying *right
/// now, again* has any chance of helping after the budget was already spent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureRetryReport {
    /// Logical operation whose retry loop exhausted (e.g. `create_message`).
    pub operation: String,
    /// Attempts actually made (initial attempt + retries).
    pub attempts_made: u32,
    /// Total attempt budget that was available.
    pub retry_budget: u32,
    /// Wall-clock time spent inside the retry loop, in milliseconds.
    pub elapsed_wait_ms: u64,
    /// Whether the budget was fully spent (always true when this is present).
    pub budget_exhausted: bool,
    /// Whether one more immediate retry is plausibly useful.
    pub immediate_retry_useful: bool,
    /// Exact fallback guidance for the caller.
    pub next_action: String,
}

/// Best-effort lock-owner identity parsed from the failure detail (D3).
///
/// Populated for `LiveOwnerNoActivityLock` refusals (and for `BusyRetryable`
/// when the detail names a PID). `command`/`alive` come from side-effect-free
/// `/proc/<pid>` reads on Linux and are `None` elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureLockOwner {
    /// Owner PID when the error detail names one.
    pub pid: Option<u32>,
    /// Owner process command name (`/proc/<pid>/comm`), best-effort.
    pub command: Option<String>,
    /// Whether the owner PID is currently alive, best-effort.
    pub alive: Option<bool>,
    /// Provenance of this owner identity.
    pub source: &'static str,
}

/// File-descriptor pressure snapshot for `FdExhaustion` failures (D3).
///
/// Captured with side-effect-free `/proc/self` reads on Linux (`None`
/// elsewhere). `eviction_freed` is parsed from the error detail when the
/// failing path reported a repo-cache eviction outcome ("Freed N cached
/// repos"); a value of `Some(0)` means eviction freed nothing, so another
/// blind retry will deterministically fail — `immediate_retry_useful` is
/// `false` and `next_action` gives a non-retry step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureFdPressure {
    /// Soft `RLIMIT_NOFILE` limit (`None` when unknown or unlimited).
    pub soft_limit: Option<u64>,
    /// Hard `RLIMIT_NOFILE` limit (`None` when unknown or unlimited).
    pub hard_limit: Option<u64>,
    /// Approximate open descriptor count for this process (includes the
    /// probe's own directory descriptor).
    pub open_fds: Option<u64>,
    /// Repo-cache entries freed by eviction, parsed from the error detail.
    pub eviction_freed: Option<u64>,
    /// Whether one more immediate retry is plausibly useful.
    pub immediate_retry_useful: bool,
    /// Exact fallback guidance for the caller.
    pub next_action: String,
}

/// Single structured failure envelope for database and DB-adjacent failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DbFailureEnvelope {
    pub schema_version: &'static str,
    pub class: &'static str,
    pub severity: &'static str,
    pub error_code: &'static str,
    pub error_detail: String,
    pub effective_db_path: Option<String>,
    pub effective_storage_root: Option<String>,
    pub process: DbFailureProcessIdentity,
    pub wal_mode: DbFailureProbeResult,
    pub sidecars: DbFailureSidecars,
    pub canonical_sqlite_probe: DbFailureProbeResult,
    pub frankensqlite_probe: DbFailureProbeResult,
    pub last_successful_health_ts: Option<String>,
    pub last_successful_write_ts: Option<String>,
    pub legacy_python_present: Option<bool>,
    pub tui_polling_active: Option<bool>,
    pub host: DbFailureHostSummary,
    pub policy: DbFailurePolicy,
    /// Bounded-retry report; present when a retry loop exhausted its budget.
    pub retry: Option<DbFailureRetryReport>,
    /// Best-effort lock-owner identity for busy/live-owner contention.
    pub lock_owner: Option<DbFailureLockOwner>,
    /// FD-pressure snapshot; present for `fd_exhaustion` failures.
    pub fd_pressure: Option<DbFailureFdPressure>,
}

impl DbFailureEnvelope {
    /// Build a failure-tolerant envelope without opening or mutating SQLite.
    #[must_use]
    pub fn from_error(error: &DbError) -> Self {
        let classification = error.classification();
        let (db_path, storage_root) = effective_db_environment();
        let detail = error.to_string();
        let retry = retry_report_for(error, classification.class);
        let lock_owner = lock_owner_for(classification.class, &detail);
        let fd_pressure = fd_pressure_for(classification.class, &detail);

        Self {
            schema_version: DB_FAILURE_ENVELOPE_SCHEMA_VERSION,
            class: classification.class.as_str(),
            severity: classification.severity.as_str(),
            error_code: error.error_code(),
            error_detail: error.to_string(),
            effective_db_path: db_path.as_deref().map(path_to_string),
            effective_storage_root: storage_root.as_deref().map(path_to_string),
            process: process_identity(),
            wal_mode: DbFailureProbeResult {
                status: "not_collected",
                detail: Some(
                    "error-path envelope does not open SQLite; run `am doctor health` for live WAL mode"
                        .to_string(),
                ),
            },
            sidecars: sidecar_snapshot(db_path.as_deref()),
            canonical_sqlite_probe: DbFailureProbeResult {
                status: "not_collected",
                detail: Some(
                    "canonical SQLite probe is intentionally side-effect-free in this formatter"
                        .to_string(),
                ),
            },
            frankensqlite_probe: DbFailureProbeResult {
                status: "classified_from_error",
                detail: Some(error.to_string()),
            },
            last_successful_health_ts: None,
            last_successful_write_ts: None,
            legacy_python_present: None,
            tui_polling_active: None,
            host: host_summary(),
            policy: DbFailurePolicy::from(classification),
            retry,
            lock_owner,
            fd_pressure,
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
    ///
    /// A [`Self::RetryBudgetExhausted`] wrapper delegates to the wrapped
    /// error: the *family* stays retryable even though one loop's budget was
    /// spent (callers should consult the failure envelope's `retry` section
    /// before retrying immediately).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Sqlite(msg) | Self::Pool(msg) | Self::Schema(msg) | Self::Internal(msg) => {
                classify_db_error_message(msg).safe_to_retry
            }
            Self::ResourceBusy(_) | Self::PoolExhausted { .. } => true,
            Self::RetryBudgetExhausted { inner, .. } => inner.is_retryable(),
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
            Self::RetryBudgetExhausted { inner, .. } => inner.is_corruption(),
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
                } else if contains_main_db_corruption(message)
                    || contains_main_db_corruption(&joined_details)
                {
                    DbErrorClass::MainDbBtreeCorruption
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
            Self::RetryBudgetExhausted { inner, .. } => inner.classification(),
            Self::NotFound { .. }
            | Self::Duplicate { .. }
            | Self::InvalidArgument { .. }
            | Self::Serialization(_) => {
                DbErrorClassification::for_class(DbErrorClass::ConnectionOrConfigError)
            }
        }
    }

    /// Structured robot-facing failure envelope for this error.
    #[must_use]
    pub fn failure_envelope(&self) -> DbFailureEnvelope {
        DbFailureEnvelope::from_error(self)
    }

    /// The legacy error code string for this error.
    ///
    /// A [`Self::RetryBudgetExhausted`] wrapper reports the wrapped error's
    /// code so legacy consumers keep seeing `RESOURCE_BUSY` /
    /// `DATABASE_POOL_EXHAUSTED` rather than a new opaque code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::PoolExhausted { .. } => "DATABASE_POOL_EXHAUSTED",
            Self::ResourceBusy(_) | Self::CircuitBreakerOpen { .. } => "RESOURCE_BUSY",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::Duplicate { .. } => "DUPLICATE",
            Self::InvalidArgument { .. } => "INVALID_ARGUMENT",
            Self::IntegrityCorruption { .. } => "INTEGRITY_CORRUPTION",
            Self::RetryBudgetExhausted { inner, .. } => inner.error_code(),
            _ => "INTERNAL_ERROR",
        }
    }

    /// Whether the error is recoverable (client can retry).
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::PoolExhausted { .. }
            | Self::ResourceBusy(_)
            | Self::CircuitBreakerOpen { .. }
            | Self::Pool(_) => true,
            Self::RetryBudgetExhausted { inner, .. } => inner.is_recoverable(),
            _ => false,
        }
    }
}

fn effective_db_environment() -> (Option<PathBuf>, Option<PathBuf>) {
    std::panic::catch_unwind(|| {
        let config = mcp_agent_mail_core::Config::from_env();
        let db_path = mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(
            config.database_url.as_str(),
        );
        (db_path, Some(config.storage_root))
    })
    .unwrap_or((None, None))
}

fn path_to_string(path: &Path) -> String {
    path.display().to_string()
}

fn process_identity() -> DbFailureProcessIdentity {
    DbFailureProcessIdentity {
        pid: std::process::id(),
        binary_path: std::env::current_exe()
            .ok()
            .map(|path| path.display().to_string()),
        binary_version: env!("CARGO_PKG_VERSION"),
    }
}

fn sidecar_snapshot(db_path: Option<&Path>) -> DbFailureSidecars {
    let wal = db_path.map(|path| sidecar_state(&PathBuf::from(format!("{}-wal", path.display()))));
    let shm = db_path.map(|path| sidecar_state(&PathBuf::from(format!("{}-shm", path.display()))));

    DbFailureSidecars {
        wal: wal.unwrap_or_else(|| absent_sidecar_state(None)),
        shm: shm.unwrap_or_else(|| absent_sidecar_state(None)),
    }
}

fn sidecar_state(path: &Path) -> DbFailureSidecarState {
    let path_text = Some(path.display().to_string());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified_unix_ms = metadata.modified().ok().and_then(system_time_unix_ms);
            let modified_age_ms = metadata.modified().ok().and_then(system_time_age_ms);
            DbFailureSidecarState {
                path: path_text,
                exists: true,
                len_bytes: Some(metadata.len()),
                modified_unix_ms,
                modified_age_ms,
                inspect_error: None,
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            absent_sidecar_state(path_text)
        }
        Err(error) => DbFailureSidecarState {
            path: path_text,
            exists: false,
            len_bytes: None,
            modified_unix_ms: None,
            modified_age_ms: None,
            inspect_error: Some(error.to_string()),
        },
    }
}

fn absent_sidecar_state(path: Option<String>) -> DbFailureSidecarState {
    DbFailureSidecarState {
        path,
        exists: false,
        len_bytes: None,
        modified_unix_ms: None,
        modified_age_ms: None,
        inspect_error: None,
    }
}

fn system_time_unix_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

fn system_time_age_ms(time: SystemTime) -> Option<u64> {
    SystemTime::now()
        .duration_since(time)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

fn host_summary() -> DbFailureHostSummary {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(loadavg) => DbFailureHostSummary {
            loadavg: Some(loadavg.trim().to_string()),
            disk_summary: None,
            inspect_error: None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DbFailureHostSummary {
            loadavg: None,
            disk_summary: None,
            inspect_error: None,
        },
        Err(error) => DbFailureHostSummary {
            loadavg: None,
            disk_summary: None,
            inspect_error: Some(error.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// D3: bounded-retry / lock-owner / FD-pressure envelope sections
// ---------------------------------------------------------------------------

fn retry_report_for(error: &DbError, class: DbErrorClass) -> Option<DbFailureRetryReport> {
    let DbError::RetryBudgetExhausted {
        operation,
        attempts,
        budget,
        elapsed_ms,
        inner,
    } = error
    else {
        return None;
    };
    Some(DbFailureRetryReport {
        operation: (*operation).to_string(),
        attempts_made: *attempts,
        retry_budget: *budget,
        elapsed_wait_ms: *elapsed_ms,
        budget_exhausted: true,
        immediate_retry_useful: false,
        next_action: exhausted_next_action(class, &inner.to_string()),
    })
}

/// Class-specific fallback guidance once a retry budget is exhausted.
fn exhausted_next_action(class: DbErrorClass, inner_detail: &str) -> String {
    match class {
        DbErrorClass::BusyRetryable => "Do not immediately retry: the server already retried \
             this operation. Run `am doctor locks --json` to identify the lock holder, wait \
             for it to clear, then retry once."
            .to_string(),
        DbErrorClass::LiveOwnerNoActivityLock => "Do not retry the direct write. A running \
             Agent Mail server owns this mailbox; route the operation through that server \
             (see `am doctor locks --json` for the owner)."
            .to_string(),
        DbErrorClass::PoolExhaustion => "Reduce concurrent Agent Mail callers or raise pool \
             capacity, then retry. Inspect `am robot metrics` for pool wait times."
            .to_string(),
        DbErrorClass::FdExhaustion => fd_next_action(fd_eviction_freed(inner_detail)),
        other => format!(
            "Run `{}`.",
            DbErrorClassification::for_class(other).recommended_command
        ),
    }
}

fn lock_owner_for(class: DbErrorClass, detail: &str) -> Option<DbFailureLockOwner> {
    let pid = parse_owner_pid(detail);
    match class {
        // A live-owner refusal always has an owner, even when the detail did
        // not name a PID; surface the section so agents know one exists.
        DbErrorClass::LiveOwnerNoActivityLock => Some(build_lock_owner(pid)),
        DbErrorClass::BusyRetryable => pid.map(|p| build_lock_owner(Some(p))),
        _ => None,
    }
}

fn build_lock_owner(pid: Option<u32>) -> DbFailureLockOwner {
    let (command, alive) = pid.map_or((None, None), owner_process_info);
    DbFailureLockOwner {
        pid,
        command,
        alive,
        source: "parsed_from_error_detail",
    }
}

/// Parse a PID out of an error detail ("pid 4242", "pid=17", "pid: 9").
///
/// Requires a word boundary before "pid" so words like "rapid" do not match.
fn parse_owner_pid(detail: &str) -> Option<u32> {
    let lower = detail.to_lowercase();
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(pos) = lower[search_from..].find("pid") {
        let start = search_from + pos;
        let boundary_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after = &lower[start + 3..];
        if boundary_ok {
            let after =
                after.trim_start_matches(|c: char| c == '=' || c == ':' || c.is_whitespace());
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty()
                && let Ok(pid) = digits.parse()
            {
                return Some(pid);
            }
        }
        search_from = start + 3;
    }
    None
}

#[cfg(target_os = "linux")]
fn owner_process_info(pid: u32) -> (Option<String>, Option<bool>) {
    let alive = std::fs::metadata(format!("/proc/{pid}")).is_ok();
    let command = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (command, Some(alive))
}

#[cfg(not(target_os = "linux"))]
fn owner_process_info(_pid: u32) -> (Option<String>, Option<bool>) {
    (None, None)
}

fn fd_pressure_for(class: DbErrorClass, detail: &str) -> Option<DbFailureFdPressure> {
    if class != DbErrorClass::FdExhaustion {
        return None;
    }
    let (soft_limit, hard_limit) = read_fd_limits();
    let eviction_freed = fd_eviction_freed(detail);
    Some(DbFailureFdPressure {
        soft_limit,
        hard_limit,
        open_fds: count_open_fds(),
        eviction_freed,
        immediate_retry_useful: eviction_freed != Some(0),
        next_action: fd_next_action(eviction_freed),
    })
}

fn fd_next_action(eviction_freed: Option<u64>) -> String {
    match eviction_freed {
        Some(0) => "Repo-cache eviction freed 0 entries, so retrying will fail again. Close \
             stale Agent Mail processes, raise the open-file limit (ulimit -n), or restart \
             the owning server, then run `am doctor health`."
            .to_string(),
        Some(n) => format!(
            "Repo-cache eviction freed {n} entries; one immediate retry is reasonable. If FD \
             pressure recurs, raise the open-file limit (ulimit -n) or reduce concurrent agents."
        ),
        None => "Compare open_fds against soft_limit; close stale Agent Mail processes or \
             raise the open-file limit (ulimit -n) before retrying."
            .to_string(),
    }
}

/// Parse the repo-cache eviction outcome from an error detail.
///
/// Recognizes the "Freed N cached repos" phrasing emitted by FD-pressure
/// eviction paths (and preserved in historical session anchors). Returns
/// `None` when the detail does not report an eviction outcome.
#[must_use]
pub fn fd_eviction_freed(detail: &str) -> Option<u64> {
    let lower = detail.to_lowercase();
    if !lower.contains("cached repo") && !lower.contains("repo cache") {
        return None;
    }
    let pos = lower.find("freed")?;
    let after = lower[pos + "freed".len()..].trim_start();
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(target_os = "linux")]
fn read_fd_limits() -> (Option<u64>, Option<u64>) {
    let Ok(limits) = std::fs::read_to_string("/proc/self/limits") else {
        return (None, None);
    };
    for line in limits.lines() {
        if let Some(rest) = line.strip_prefix("Max open files") {
            let mut fields = rest.split_whitespace();
            let soft = fields.next().and_then(|v| v.parse().ok());
            let hard = fields.next().and_then(|v| v.parse().ok());
            return (soft, hard);
        }
    }
    (None, None)
}

#[cfg(not(target_os = "linux"))]
fn read_fd_limits() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(target_os = "linux")]
fn count_open_fds() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .and_then(|entries| u64::try_from(entries.count()).ok())
}

#[cfg(not(target_os = "linux"))]
fn count_open_fds() -> Option<u64> {
    None
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

/// Check whether an error message indicates authoritative main database
/// corruption that may be recoverable via backup restore or archive
/// reconstruction.
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
    if contains_schema_drift(msg) {
        return DbErrorClass::SchemaDriftOrMissingTables;
    }
    if contains_engine_probe_limitation(msg) {
        return DbErrorClass::EngineProbeLimitation;
    }
    if contains_main_db_corruption(msg) {
        return DbErrorClass::MainDbBtreeCorruption;
    }
    if contains_foreign_key_inconsistency(msg) {
        return DbErrorClass::ForeignKeyInconsistency;
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
        || lower.contains("malformed database schema")
        || lower.contains("database schema is corrupt")
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
            "malformed database schema (idx_agent_links_pair_unique) - invalid rootpage (11)",
            DbErrorClass::SchemaDriftOrMissingTables,
        );
        assert!(!is_corruption_error(
            "malformed database schema (idx_agent_links_pair_unique) - invalid rootpage (11)"
        ));
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

        let authoritative_integrity = DbError::IntegrityCorruption {
            message: "integrity failed".into(),
            details: vec![
                "malformed database schema (idx_agent_links_pair_unique) - invalid rootpage (11)"
                    .into(),
            ],
        };
        assert_eq!(
            authoritative_integrity.classification().class,
            DbErrorClass::MainDbBtreeCorruption
        );
    }

    #[test]
    fn failure_envelope_serializes_stable_a2_robot_shape() {
        let error = DbError::ResourceBusy("database is locked".into());
        let envelope = error.failure_envelope();
        assert_eq!(envelope.schema_version, DB_FAILURE_ENVELOPE_SCHEMA_VERSION);
        assert_eq!(envelope.class, "busy_retryable");
        assert_eq!(envelope.severity, "P2");
        assert_eq!(envelope.error_code, "RESOURCE_BUSY");
        assert!(envelope.policy.safe_to_retry);
        assert!(envelope.policy.blocks_edits);
        assert_eq!(envelope.wal_mode.status, "not_collected");
        assert_eq!(envelope.frankensqlite_probe.status, "classified_from_error");

        let value = serde_json::to_value(&envelope).expect("envelope serializes");
        assert_eq!(value["schema_version"], DB_FAILURE_ENVELOPE_SCHEMA_VERSION);
        assert_eq!(value["class"], "busy_retryable");
        assert!(value["process"]["pid"].as_u64().is_some());
        assert!(value["sidecars"]["wal"].get("exists").is_some());
        assert!(value["sidecars"]["shm"].get("exists").is_some());
        assert!(value["canonical_sqlite_probe"]["status"].is_string());
        assert!(value["host"].get("loadavg").is_some());
        assert_eq!(
            value["policy"]["recommended_command"],
            envelope.policy.recommended_command
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
        assert!(!is_corruption_error("malformed database schema: agents"));
        assert!(!is_corruption_error("database schema is corrupt"));
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

    // ── D3: retry budget, lock owner, FD pressure ────────────────────

    fn wrap_exhausted(inner: DbError) -> DbError {
        DbError::RetryBudgetExhausted {
            operation: "create_message",
            attempts: 17,
            budget: 17,
            elapsed_ms: 29_000,
            inner: Box::new(inner),
        }
    }

    #[test]
    fn retry_budget_exhausted_delegates_classification_and_codes() {
        let busy = wrap_exhausted(DbError::ResourceBusy("database is locked".into()));
        assert_eq!(busy.classification().class, DbErrorClass::BusyRetryable);
        assert_eq!(busy.error_code(), "RESOURCE_BUSY");
        assert!(busy.is_retryable());
        assert!(busy.is_recoverable());
        assert!(!busy.is_corruption());

        let pool = wrap_exhausted(DbError::PoolExhausted {
            message: "all connections in use".into(),
            pool_size: 4,
            max_overflow: 2,
        });
        assert_eq!(pool.classification().class, DbErrorClass::PoolExhaustion);
        assert_eq!(pool.error_code(), "DATABASE_POOL_EXHAUSTED");

        let corrupt = wrap_exhausted(DbError::Sqlite("database disk image is malformed".into()));
        assert!(corrupt.is_corruption());
        assert!(!corrupt.is_retryable());
    }

    #[test]
    fn retry_budget_exhausted_display_preserves_inner_detail() {
        let e = wrap_exhausted(DbError::ResourceBusy("database is locked".into()));
        let text = e.to_string();
        assert!(text.contains("create_message"), "{text}");
        assert!(text.contains("17/17 attempts"), "{text}");
        assert!(text.contains("29000 ms"), "{text}");
        assert!(text.contains("database is locked"), "{text}");
    }

    #[test]
    fn retry_budget_exhausted_envelope_has_retry_section() {
        let e = wrap_exhausted(DbError::ResourceBusy("database is locked".into()));
        let envelope = e.failure_envelope();
        let retry = envelope.retry.expect("retry section present");
        assert_eq!(retry.operation, "create_message");
        assert_eq!(retry.attempts_made, 17);
        assert_eq!(retry.retry_budget, 17);
        assert_eq!(retry.elapsed_wait_ms, 29_000);
        assert!(retry.budget_exhausted);
        assert!(!retry.immediate_retry_useful);
        assert!(retry.next_action.contains("am doctor locks --json"));

        // Class policy stays family-level; instance honesty lives in `retry`.
        assert!(envelope.policy.safe_to_retry);
        assert_eq!(envelope.class, "busy_retryable");
    }

    #[test]
    fn plain_errors_have_no_retry_section() {
        let e = DbError::ResourceBusy("database is locked".into());
        let envelope = e.failure_envelope();
        assert!(envelope.retry.is_none());
        assert!(envelope.fd_pressure.is_none());
    }

    #[test]
    fn live_owner_envelope_names_parsed_owner() {
        let e = DbError::ResourceBusy(
            "Another active process owns this mailbox (pid 4242). \
             Route writes through that process or stop it first."
                .into(),
        );
        let envelope = e.failure_envelope();
        assert_eq!(envelope.class, "live_owner_no_activity_lock");
        let owner = envelope.lock_owner.expect("owner section present");
        assert_eq!(owner.pid, Some(4242));
        assert_eq!(owner.source, "parsed_from_error_detail");
    }

    #[test]
    fn live_owner_envelope_present_even_without_pid() {
        let e = DbError::ResourceBusy(
            "mailbox mutation refused: another server owns the mailbox".into(),
        );
        let envelope = e.failure_envelope();
        let owner = envelope.lock_owner.expect("owner section present");
        assert_eq!(owner.pid, None);
    }

    #[test]
    fn busy_envelope_includes_owner_only_when_pid_known() {
        let with_pid =
            DbError::ResourceBusy("database is locked by another process: pid=17".into());
        assert_eq!(
            with_pid.failure_envelope().lock_owner.and_then(|o| o.pid),
            Some(17)
        );

        let without_pid = DbError::ResourceBusy("database is locked".into());
        assert!(without_pid.failure_envelope().lock_owner.is_none());
    }

    #[test]
    fn parse_owner_pid_variants() {
        assert_eq!(
            parse_owner_pid("owner: pid 4242 mode=exclusive"),
            Some(4242)
        );
        assert_eq!(parse_owner_pid("owner hint: pid=17"), Some(17));
        assert_eq!(parse_owner_pid("holder pid: 9"), Some(9));
        assert_eq!(parse_owner_pid("PID 33 holds the lock"), Some(33));
        assert_eq!(parse_owner_pid("rapid 5 growth"), None);
        assert_eq!(parse_owner_pid("no owner named"), None);
        assert_eq!(parse_owner_pid("pid unknown"), None);
    }

    #[test]
    fn fd_eviction_freed_parses_anchor_phrasings() {
        assert_eq!(
            fd_eviction_freed("Too many open files. Freed 0 cached repos"),
            Some(0)
        );
        assert_eq!(
            fd_eviction_freed("Too many open files. Freed 1 cached repos. Retry"),
            Some(1)
        );
        assert_eq!(
            fd_eviction_freed("too many open files. freed 12 cached repos"),
            Some(12)
        );
        // No eviction outcome reported.
        assert_eq!(fd_eviction_freed("Too many open files (os error 24)"), None);
        // "freed" without the repo-cache phrase is not an eviction outcome.
        assert_eq!(fd_eviction_freed("freed 3 buffers"), None);
    }

    #[test]
    fn fd_exhaustion_envelope_freed_zero_is_not_retry_useful() {
        let e = DbError::Internal(
            "send_message retry loop exhausted: Too many open files. Freed 0 cached repos".into(),
        );
        let envelope = e.failure_envelope();
        assert_eq!(envelope.class, "fd_exhaustion");
        let fd = envelope.fd_pressure.expect("fd section present");
        assert_eq!(fd.eviction_freed, Some(0));
        assert!(!fd.immediate_retry_useful);
        assert!(fd.next_action.contains("retrying will fail again"));

        #[cfg(target_os = "linux")]
        {
            assert!(fd.soft_limit.is_some(), "soft limit on linux: {fd:?}");
            assert!(fd.open_fds.is_some(), "open fds on linux: {fd:?}");
        }
    }

    #[test]
    fn fd_exhaustion_envelope_with_successful_eviction_is_retry_useful() {
        let e = DbError::Internal("Too many open files. Freed 2 cached repos".into());
        let fd = e
            .failure_envelope()
            .fd_pressure
            .expect("fd section present");
        assert_eq!(fd.eviction_freed, Some(2));
        assert!(fd.immediate_retry_useful);
    }

    #[test]
    fn fd_exhaustion_wrapped_retry_next_action_is_freed_aware() {
        let e = wrap_exhausted(DbError::Internal(
            "Too many open files. Freed 0 cached repos".into(),
        ));
        let envelope = e.failure_envelope();
        let retry = envelope.retry.expect("retry section present");
        assert!(
            retry.next_action.contains("retrying will fail again"),
            "freed-0 next action: {}",
            retry.next_action
        );
        let fd = envelope.fd_pressure.expect("fd section present");
        assert_eq!(fd.eviction_freed, Some(0));
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
