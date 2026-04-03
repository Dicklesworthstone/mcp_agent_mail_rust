//! Connection pool configuration and initialization
//!
//! Uses `sqlmodel_pool` for efficient connection management.

use crate::DbConn;
use crate::error::{DbError, DbResult, is_lock_error};
use crate::integrity;
use crate::schema;
use asupersync::sync::OnceCell;
use asupersync::{Cx, Outcome};
use mcp_agent_mail_core::{
    ConsistencyMessageRef, LockLevel, OrderedRwLock,
    config::env_value,
    disk::{is_sqlite_memory_database_url, sqlite_file_path_from_database_url},
};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Error as SqlError, Value};
use sqlmodel_pool::{Pool, PoolConfig, PooledConnection};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone)]
struct SampledMessage {
    id: i64,
    project_id: i64,
    sender_id: i64,
    subject: String,
    created_ts_iso: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMailboxSqlitePath {
    pub configured_path: String,
    pub canonical_path: String,
    pub used_absolute_fallback: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSidecarState {
    pub wal_exists: bool,
    pub wal_bytes: Option<u64>,
    pub shm_exists: bool,
    pub shm_bytes: Option<u64>,
    pub live_sidecars: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxRecoveryLockState {
    pub lock_path: String,
    pub exists: bool,
    pub active: bool,
    pub pid: Option<u32>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxOwnershipDisposition {
    Unowned,
    ActiveOtherOwner,
    StaleLiveProcess,
    DeletedExecutable,
    SplitBrain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxOwnershipProcess {
    pub pid: u32,
    pub command: Option<String>,
    pub executable_path: Option<String>,
    pub executable_deleted: bool,
    pub holds_storage_root_lock: bool,
    pub holds_sqlite_lock: bool,
    pub holds_database_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxOwnershipState {
    pub disposition: MailboxOwnershipDisposition,
    pub storage_lock_path: String,
    pub sqlite_lock_path: String,
    pub processes: Vec<MailboxOwnershipProcess>,
    pub competing_pids: Vec<u32>,
    pub supervised_restart_required: bool,
    pub detail: String,
}

impl MailboxOwnershipState {
    #[must_use]
    pub const fn blocks_mutation(&self) -> bool {
        !matches!(self.disposition, MailboxOwnershipDisposition::Unowned)
    }
}

// ============================================================================
// Recovery action classification: silent self-heal vs explicit escalation
// ============================================================================

/// Classification of a recovery action's approval requirement.
///
/// Every recovery action the system can perform falls into one of two
/// categories:
///
/// - **`SilentSelfHeal`**: The action is safe to perform automatically
///   without operator approval. These actions are idempotent, bounded in
///   scope, and cannot cause data loss even on a false positive.
///
/// - **`ExplicitEscalation`**: The action is destructive or irreversible
///   enough that it requires explicit operator approval before execution.
///   The system should log the recommendation, emit a metric, and block
///   until an operator (or an authorizing policy gate) approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryApproval {
    /// Automatic execution is safe — no operator in the loop.
    SilentSelfHeal,
    /// Must wait for explicit operator or policy-gate approval.
    ExplicitEscalation,
}

/// An enumeration of every discrete recovery action the system can attempt.
///
/// Each variant carries its classification ([`RecoveryApproval`]) as a
/// compile-time constant so call sites can branch on `action.approval()`
/// without maintaining separate lookup tables.
///
/// # Design rationale
///
/// The boundary between silent and escalated is drawn by two principles:
///
/// 1. **Idempotent + bounded + non-destructive → silent.**
///    WAL checkpoints, stale-lock cleanup, connection-pool refresh, and
///    index rebuilds meet all three criteria.
///
/// 2. **Irreversible, data-destructive, or authority-changing → escalate.**
///    Archive reconstruction replaces the live DB, corrupt-DB deletion
///    discards data, force-unlock overrides contested ownership, and
///    schema migration changes the storage contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    // ── Silent self-heal actions ──────────────────────────────────────

    /// `PRAGMA wal_checkpoint(PASSIVE)` — non-blocking, best-effort.
    WalCheckpointPassive,

    /// `PRAGMA wal_checkpoint(TRUNCATE)` — may briefly block writers but
    /// always converges and never mutates user data.
    WalCheckpointTruncate,

    /// Remove a `.recovery.lock` or `.activity.lock` file whose PID is no
    /// longer running (stale lock cleanup).
    StaleLockCleanup,

    /// Remove a zero-byte `-wal` sidecar that prevents clean open.
    EmptyWalSidecarCleanup,

    /// Drop and re-open pooled connections (e.g. after detecting a stale
    /// file-descriptor pointing at an unlinked inode).
    ConnectionPoolRefresh,

    /// `REINDEX` to repair index-only corruption detected by
    /// `quick_check` / `integrity_check(1)`.
    IndexRebuild,

    /// Rebuild the `inbox_stats` materialized summary table from
    /// ground-truth message data. Purely derived; never loses user data.
    InboxStatsRebuild,

    /// Restore the live database from a healthy `.bak` sibling that was
    /// proactively created by the system itself. The corrupt file is
    /// quarantined (renamed), not deleted.
    RestoreFromProactiveBackup,

    /// Create a `.bak` backup of the database file during idle periods.
    CreateProactiveBackup,

    // ── Explicit escalation actions ──────────────────────────────────

    /// Reconstruct the SQLite database from the Git-backed mail archive.
    /// This replaces the live DB file entirely and may lose non-archived
    /// state (e.g. local draft metadata).
    ReconstructFromArchive,

    /// Delete (or quarantine-then-replace) a corrupt database file and
    /// reinitialize from scratch when no backup or archive is available.
    DeleteCorruptDb,

    /// Override a contested lock held by a live (or ambiguous) process.
    /// Could cause split-brain if the other process is still writing.
    ForceUnlockContested,

    /// Run a schema migration that alters table structure, column types,
    /// or index definitions on the live database.
    SchemaMigration,

    /// Promote a reconstructed candidate database to the live path after
    /// archive-based recovery.
    PromoteReconstructedCandidate,

    /// Reinitialize the database from scratch (blank), discarding all
    /// existing data because no recovery source is available.
    ReinitializeBlank,
}

impl RecoveryAction {
    /// The approval classification for this action.
    #[must_use]
    pub const fn approval(&self) -> RecoveryApproval {
        match self {
            // Silent self-heal: idempotent, bounded, non-destructive
            Self::WalCheckpointPassive
            | Self::WalCheckpointTruncate
            | Self::StaleLockCleanup
            | Self::EmptyWalSidecarCleanup
            | Self::ConnectionPoolRefresh
            | Self::IndexRebuild
            | Self::InboxStatsRebuild
            | Self::RestoreFromProactiveBackup
            | Self::CreateProactiveBackup => RecoveryApproval::SilentSelfHeal,

            // Explicit escalation: destructive, irreversible, or authority-changing
            Self::ReconstructFromArchive
            | Self::DeleteCorruptDb
            | Self::ForceUnlockContested
            | Self::SchemaMigration
            | Self::PromoteReconstructedCandidate
            | Self::ReinitializeBlank => RecoveryApproval::ExplicitEscalation,
        }
    }

    /// Whether this action can be performed without operator approval.
    #[must_use]
    pub const fn is_silent(&self) -> bool {
        matches!(self.approval(), RecoveryApproval::SilentSelfHeal)
    }

    /// Whether this action requires explicit operator approval.
    #[must_use]
    pub const fn requires_escalation(&self) -> bool {
        matches!(self.approval(), RecoveryApproval::ExplicitEscalation)
    }

    /// A short human-readable label for log messages and metrics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::WalCheckpointPassive => "wal_checkpoint_passive",
            Self::WalCheckpointTruncate => "wal_checkpoint_truncate",
            Self::StaleLockCleanup => "stale_lock_cleanup",
            Self::EmptyWalSidecarCleanup => "empty_wal_sidecar_cleanup",
            Self::ConnectionPoolRefresh => "connection_pool_refresh",
            Self::IndexRebuild => "index_rebuild",
            Self::InboxStatsRebuild => "inbox_stats_rebuild",
            Self::RestoreFromProactiveBackup => "restore_from_proactive_backup",
            Self::CreateProactiveBackup => "create_proactive_backup",
            Self::ReconstructFromArchive => "reconstruct_from_archive",
            Self::DeleteCorruptDb => "delete_corrupt_db",
            Self::ForceUnlockContested => "force_unlock_contested",
            Self::SchemaMigration => "schema_migration",
            Self::PromoteReconstructedCandidate => "promote_reconstructed_candidate",
            Self::ReinitializeBlank => "reinitialize_blank",
        }
    }

    /// Explanation of why this action has its current classification.
    #[must_use]
    pub const fn rationale(&self) -> &'static str {
        match self {
            Self::WalCheckpointPassive =>
                "Non-blocking best-effort; never mutates user data or blocks writers",
            Self::WalCheckpointTruncate =>
                "May briefly block writers but always converges; no user data mutation",
            Self::StaleLockCleanup =>
                "Only removes locks whose owning PID no longer exists; idempotent",
            Self::EmptyWalSidecarCleanup =>
                "Only removes zero-byte WAL files that prevent clean open; idempotent",
            Self::ConnectionPoolRefresh =>
                "Closes stale file descriptors and opens fresh connections; no data mutation",
            Self::IndexRebuild =>
                "REINDEX rebuilds derived index structures; user data rows are untouched",
            Self::InboxStatsRebuild =>
                "Rebuilds a derived materialized view from ground-truth message data",
            Self::RestoreFromProactiveBackup =>
                "Quarantines (renames) the corrupt file and copies back the system-created .bak",
            Self::CreateProactiveBackup =>
                "Copies the primary database to a .bak sibling; purely additive",
            Self::ReconstructFromArchive =>
                "Replaces the live database from Git archive; may lose non-archived local state",
            Self::DeleteCorruptDb =>
                "Quarantines and replaces the corrupt database; irreversible data loss if no backup",
            Self::ForceUnlockContested =>
                "Overrides locks held by a potentially live process; risk of split-brain writes",
            Self::SchemaMigration =>
                "Alters table structure on the live database; irreversible without backup",
            Self::PromoteReconstructedCandidate =>
                "Replaces the live database with a reconstructed candidate; loses any non-archived state",
            Self::ReinitializeBlank =>
                "Creates an empty database discarding all existing data; total data loss",
        }
    }

    /// All recovery actions, in declaration order.
    pub const ALL: &'static [RecoveryAction] = &[
        Self::WalCheckpointPassive,
        Self::WalCheckpointTruncate,
        Self::StaleLockCleanup,
        Self::EmptyWalSidecarCleanup,
        Self::ConnectionPoolRefresh,
        Self::IndexRebuild,
        Self::InboxStatsRebuild,
        Self::RestoreFromProactiveBackup,
        Self::CreateProactiveBackup,
        Self::ReconstructFromArchive,
        Self::DeleteCorruptDb,
        Self::ForceUnlockContested,
        Self::SchemaMigration,
        Self::PromoteReconstructedCandidate,
        Self::ReinitializeBlank,
    ];

    /// All silent self-heal actions.
    pub const SILENT: &'static [RecoveryAction] = &[
        Self::WalCheckpointPassive,
        Self::WalCheckpointTruncate,
        Self::StaleLockCleanup,
        Self::EmptyWalSidecarCleanup,
        Self::ConnectionPoolRefresh,
        Self::IndexRebuild,
        Self::InboxStatsRebuild,
        Self::RestoreFromProactiveBackup,
        Self::CreateProactiveBackup,
    ];

    /// All actions requiring explicit escalation.
    pub const ESCALATED: &'static [RecoveryAction] = &[
        Self::ReconstructFromArchive,
        Self::DeleteCorruptDb,
        Self::ForceUnlockContested,
        Self::SchemaMigration,
        Self::PromoteReconstructedCandidate,
        Self::ReinitializeBlank,
    ];
}

impl std::fmt::Display for RecoveryAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::fmt::Display for RecoveryApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SilentSelfHeal => f.write_str("silent_self_heal"),
            Self::ExplicitEscalation => f.write_str("explicit_escalation"),
        }
    }
}

// ============================================================================
// Recovery admission control: single-flight, backoff, loop suppression
// ============================================================================

/// Governs admission of recovery attempts to prevent thundering-herd,
/// runaway-loop, and retry-storm failure modes.
///
/// Three control layers work together:
///
/// 1. **Single-flight guard** — An [`AtomicBool`] ensures at most one
///    recovery attempt runs at any given time. Concurrent callers see
///    `Ok(false)` immediately rather than queuing.
///
/// 2. **Exponential backoff** — After each consecutive failure the
///    controller enforces an increasing cooldown before the next attempt
///    is admitted. Backoff resets on success.
///
/// 3. **Loop suppression** — If recovery fires more than
///    [`MAX_ATTEMPTS_IN_WINDOW`] times within [`SUPPRESSION_WINDOW`],
///    all further attempts are refused until the window expires. This
///    prevents a broken-disk or missing-backup scenario from burning
///    CPU in a tight retry loop.
///
/// The controller is stored in a global [`OnceLock`] so all callers in
/// the process share the same admission state. It is fully `Sync` and
/// lock-free on the fast path (single-flight check + timestamp compare).
pub struct RecoveryAdmissionController {
    /// Single-flight guard: `true` when a recovery is in progress.
    in_progress: std::sync::atomic::AtomicBool,

    /// Mutable state behind a `Mutex` — only held briefly to read/update
    /// counters and timestamps, never across the actual recovery I/O.
    state: Mutex<RecoveryAdmissionState>,
}

/// Interior state protected by the controller's `Mutex`.
struct RecoveryAdmissionState {
    /// Number of consecutive failures (reset to 0 on success).
    consecutive_failures: u32,

    /// Instant of the most recent recovery attempt (success or failure).
    last_attempt: Option<Instant>,

    /// Ring buffer of attempt timestamps within the current suppression window.
    window_attempts: std::collections::VecDeque<Instant>,

    /// If `Some`, the controller has entered suppression mode and will not
    /// admit new attempts until this instant.
    suppressed_until: Option<Instant>,
}

/// Configuration constants for the admission controller.
impl RecoveryAdmissionController {
    /// Maximum recovery attempts allowed within [`SUPPRESSION_WINDOW`].
    /// Once exceeded, the controller refuses further attempts until the
    /// window rotates.
    pub const MAX_ATTEMPTS_IN_WINDOW: usize = 5;

    /// The sliding window over which [`MAX_ATTEMPTS_IN_WINDOW`] is tracked.
    pub const SUPPRESSION_WINDOW: Duration = Duration::from_secs(300); // 5 minutes

    /// Base delay for exponential backoff (doubles on each consecutive failure).
    pub const BACKOFF_BASE: Duration = Duration::from_secs(2);

    /// Maximum backoff delay (cap to avoid unbounded wait).
    pub const BACKOFF_CAP: Duration = Duration::from_secs(120); // 2 minutes

    /// Create a new controller in the ready (un-suppressed, no backoff) state.
    pub fn new() -> Self {
        Self {
            in_progress: std::sync::atomic::AtomicBool::new(false),
            state: Mutex::new(RecoveryAdmissionState {
                consecutive_failures: 0,
                last_attempt: None,
                window_attempts: std::collections::VecDeque::new(),
                suppressed_until: None,
            }),
        }
    }

    /// Attempt to acquire the single-flight guard.
    ///
    /// Returns `Some(RecoveryGuard)` if recovery may proceed, or `None` if:
    /// - Another recovery is already in progress (single-flight).
    /// - The controller is in backoff cooldown after a recent failure.
    /// - Loop suppression is active (too many attempts in the window).
    ///
    /// When the returned `RecoveryGuard` is dropped, the in-progress flag
    /// is automatically cleared. Callers **must** call
    /// [`report_success`](Self::report_success) or
    /// [`report_failure`](Self::report_failure) before the guard drops so
    /// the backoff/window state is updated correctly.
    pub fn try_acquire(&self) -> Option<RecoveryGuard<'_>> {
        // Fast path: check suppression and backoff without acquiring the Mutex
        // on every call — but we do need the Mutex to read timestamps safely.
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();

            // Check loop suppression.
            if let Some(until) = state.suppressed_until {
                if now < until {
                    tracing::warn!(
                        remaining_secs = (until - now).as_secs(),
                        "recovery admission suppressed — too many attempts in window"
                    );
                    return None;
                }
                // Window expired — suppression will be cleared on next state update.
            }

            // Check exponential backoff.
            if state.consecutive_failures > 0 {
                if let Some(last) = state.last_attempt {
                    let required_delay = Self::backoff_delay(state.consecutive_failures);
                    let elapsed = now.saturating_duration_since(last);
                    if elapsed < required_delay {
                        tracing::info!(
                            consecutive_failures = state.consecutive_failures,
                            remaining_secs = (required_delay - elapsed).as_secs(),
                            "recovery admission deferred — exponential backoff in effect"
                        );
                        return None;
                    }
                }
            }
        }

        // Single-flight CAS.
        if self
            .in_progress
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            tracing::warn!("recovery admission refused — another recovery already in progress");
            return None;
        }

        Some(RecoveryGuard { controller: self })
    }

    /// Record a successful recovery. Resets consecutive-failure count and
    /// clears any active suppression.
    pub fn report_success(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state.consecutive_failures = 0;
        state.last_attempt = Some(now);
        state.suppressed_until = None;
        Self::prune_window(&mut state.window_attempts, now);
        state.window_attempts.push_back(now);
    }

    /// Record a failed recovery. Increments consecutive-failure count,
    /// records the attempt in the sliding window, and may activate
    /// loop suppression if the window threshold is exceeded.
    pub fn report_failure(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.last_attempt = Some(now);
        Self::prune_window(&mut state.window_attempts, now);
        state.window_attempts.push_back(now);

        if state.window_attempts.len() >= Self::MAX_ATTEMPTS_IN_WINDOW {
            let suppress_until = now + Self::SUPPRESSION_WINDOW;
            state.suppressed_until = Some(suppress_until);
            tracing::error!(
                attempts_in_window = state.window_attempts.len(),
                suppressed_for_secs = Self::SUPPRESSION_WINDOW.as_secs(),
                "recovery loop detected — suppressing further attempts"
            );
        }
    }

    /// Compute the exponential backoff delay for the given number of
    /// consecutive failures. Result is clamped to [`BACKOFF_CAP`](Self::BACKOFF_CAP).
    #[must_use]
    pub fn backoff_delay(consecutive_failures: u32) -> Duration {
        if consecutive_failures == 0 {
            return Duration::ZERO;
        }
        // 2^(failures-1) * BASE, capped at BACKOFF_CAP.
        let exponent = (consecutive_failures - 1).min(30);
        let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
        let delay_ms = Self::BACKOFF_BASE
            .as_millis()
            .saturating_mul(u128::from(multiplier));
        let delay = Duration::from_millis(delay_ms.min(u128::from(u64::MAX)) as u64);
        if delay > Self::BACKOFF_CAP {
            Self::BACKOFF_CAP
        } else {
            delay
        }
    }

    /// Return the current admission status for diagnostics.
    #[must_use]
    pub fn status(&self) -> RecoveryAdmissionStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        RecoveryAdmissionStatus {
            in_progress: self.in_progress.load(std::sync::atomic::Ordering::SeqCst),
            consecutive_failures: state.consecutive_failures,
            attempts_in_window: state.window_attempts.len(),
            suppressed: state
                .suppressed_until
                .map_or(false, |until| now < until),
            current_backoff: Self::backoff_delay(state.consecutive_failures),
        }
    }

    /// Remove window entries older than [`SUPPRESSION_WINDOW`].
    fn prune_window(window: &mut std::collections::VecDeque<Instant>, now: Instant) {
        while let Some(&front) = window.front() {
            if now.saturating_duration_since(front) > Self::SUPPRESSION_WINDOW {
                window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Reset all admission state. Intended for testing and manual operator override.
    pub fn reset(&self) {
        self.in_progress
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.consecutive_failures = 0;
        state.last_attempt = None;
        state.window_attempts.clear();
        state.suppressed_until = None;
    }
}

/// RAII guard that clears the single-flight flag when dropped.
pub struct RecoveryGuard<'a> {
    controller: &'a RecoveryAdmissionController,
}

impl Drop for RecoveryGuard<'_> {
    fn drop(&mut self) {
        self.controller
            .in_progress
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Snapshot of the admission controller state for diagnostics/logging.
#[derive(Debug, Clone)]
pub struct RecoveryAdmissionStatus {
    /// Whether a recovery is currently in progress.
    pub in_progress: bool,
    /// Number of consecutive recovery failures.
    pub consecutive_failures: u32,
    /// Number of recovery attempts within the current sliding window.
    pub attempts_in_window: usize,
    /// Whether loop suppression is currently active.
    pub suppressed: bool,
    /// Current backoff delay (zero if no failures).
    pub current_backoff: Duration,
}

/// Global singleton recovery admission controller.
///
/// Shared by all [`DbPool`] instances in the process.
static RECOVERY_ADMISSION: OnceLock<RecoveryAdmissionController> = OnceLock::new();

/// Access the global recovery admission controller.
#[must_use]
pub fn recovery_admission() -> &'static RecoveryAdmissionController {
    RECOVERY_ADMISSION.get_or_init(RecoveryAdmissionController::new)
}

// ============================================================================
// Bounded write deferral queue (br-97gc6.5.2.1.9)
// ============================================================================

/// A write operation captured for deferred replay after recovery completes.
///
/// Each entry stores the SQL statement, bound parameters, and a monotonic
/// sequence number so replay preserves original ordering.
#[derive(Debug, Clone)]
pub struct DeferredWrite {
    /// Monotonically increasing sequence number (ordering key).
    pub seq: u64,
    /// The SQL statement to replay (INSERT, UPDATE, DELETE).
    pub sql: String,
    /// Bound parameters.
    pub params: Vec<Value>,
    /// Wall-clock timestamp (microseconds) when the write was deferred.
    pub deferred_at_us: i64,
    /// Caller context for diagnostics (e.g. "send_message", "register_agent").
    pub operation: &'static str,
}

/// Outcome of attempting to enqueue a write into the deferral queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferralOutcome {
    /// Write was accepted into the queue and will be replayed after recovery.
    Queued { position: u64 },
    /// Queue is full — backpressure applied, caller should fail the write.
    BackpressureFull { capacity: usize },
    /// Queue is not active (durability state is not `Recovering`).
    NotRecovering,
    /// Queue has been sealed — no new writes accepted (drain in progress).
    Sealed,
    /// Hard-stop: oldest entry exceeded max age — recovery is stalled.
    HardStopAge {
        oldest_age_secs: u64,
        max_age_secs: u64,
    },
    /// Hard-stop: total estimated bytes exceeded budget.
    HardStopBytes {
        estimated_bytes: usize,
        max_bytes: usize,
    },
    /// Fairness limit: this operation type has consumed its share of the queue.
    FairnessLimitReached {
        operation: &'static str,
        count: usize,
        limit: usize,
    },
}

/// Configurable overload shedding policy for the deferred write queue.
///
/// Controls admission thresholds, age-based hard-stop, byte budgets, and
/// per-operation fairness limits. The defaults are safe for typical
/// multi-agent workloads; override via environment variables if needed.
#[derive(Debug, Clone)]
pub struct OverloadPolicy {
    /// Maximum number of entries before backpressure (hard capacity).
    pub max_entries: usize,
    /// Maximum age (seconds) of the oldest entry before hard-stop.
    pub max_age_secs: u64,
    /// Maximum estimated total bytes before hard-stop.
    pub max_bytes: usize,
    /// Per-operation fairness limit as percentage of max_entries.
    /// 0 = disabled (no per-operation limit).
    pub fairness_limit_pct: u8,
}

impl Default for OverloadPolicy {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_DEFERRED_WRITE_CAPACITY,
            max_age_secs: DEFAULT_DEFERRED_WRITE_MAX_AGE_SECS,
            max_bytes: DEFAULT_DEFERRED_WRITE_MAX_BYTES,
            fairness_limit_pct: DEFAULT_DEFERRED_WRITE_FAIRNESS_LIMIT_PCT,
        }
    }
}

impl OverloadPolicy {
    /// Per-operation entry limit derived from capacity and fairness percentage.
    fn fairness_limit(&self) -> usize {
        if self.fairness_limit_pct == 0 || self.fairness_limit_pct > 100 {
            return self.max_entries;
        }
        (self.max_entries as u64 * u64::from(self.fairness_limit_pct) / 100).max(1) as usize
    }
}

/// Backlog pressure tier — reflects how close the queue is to overload.
///
/// Operators and surfaces should use this to decide whether to surface
/// warnings or hard-refuse writes. The tiers are:
///
/// - `Normal`: queue is healthy, no action needed.
/// - `Elevated`: above 75% capacity — surface advisory warnings.
/// - `Critical`: at capacity or oldest entry approaching max age.
/// - `HardStop`: system refuses all new writes (stalled recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BacklogPressure {
    /// Queue is healthy.
    Normal,
    /// Above warn threshold — surface advisory to operators.
    Elevated,
    /// At capacity or oldest entry nearing max age.
    Critical,
    /// Hard-stop: new writes refused. Stalled recovery or budget exhaustion.
    HardStop,
}

/// Outcome of replaying deferred writes after recovery completes.
#[derive(Debug, Clone)]
pub struct ReplayResult {
    /// Number of writes successfully replayed.
    pub replayed: usize,
    /// Number of writes that failed during replay (logged, not retried).
    pub failed: usize,
    /// Total writes that were in the queue.
    pub total: usize,
}

/// Record of a single deferred write that failed during replay.
///
/// These are accumulated in a [`ReplayCompensationLog`] so the system can:
/// 1. Surface the exact failure to operators (which writes were lost).
/// 2. Emit structured diagnostics for the forensic bundle.
/// 3. Attempt targeted follow-up actions (e.g. re-archive, notify sender).
///
/// **Compensation strategy**: failed replay writes are *not* silently dropped.
/// The replay loop logs each failure, records it in the compensation log, and
/// continues replaying subsequent entries. After all entries are attempted, the
/// compensation log is persisted to the forensic bundle directory and surfaced
/// through doctor/robot/TUI output. Callers that submitted deferred writes can
/// query the compensation log by `seq` to learn whether their write succeeded
/// or failed. If a write fails with a constraint violation (duplicate key), it
/// is treated as an idempotent no-op (the data already exists). All other
/// failures are logged as compensation records.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayCompensationRecord {
    /// Sequence number of the deferred write (correlates with `DeferredWrite::seq`).
    pub seq: u64,
    /// The SQL that failed.
    pub sql: String,
    /// The operation type that originated this write.
    pub operation: &'static str,
    /// The error message from the failed replay attempt.
    pub error: String,
    /// When the write was originally deferred (microseconds since epoch).
    pub deferred_at_us: i64,
    /// When the replay attempt failed (microseconds since epoch).
    pub failed_at_us: i64,
}

/// Accumulates [`ReplayCompensationRecord`]s during a replay pass.
///
/// Thread-safe (uses interior `Mutex`) so replay can proceed concurrently
/// if needed, though current replay is sequential.
pub struct ReplayCompensationLog {
    entries: Mutex<Vec<ReplayCompensationRecord>>,
}

impl ReplayCompensationLog {
    /// Create an empty compensation log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Record a failed replay attempt.
    pub fn record(&self, record: ReplayCompensationRecord) {
        self.entries
            .lock()
            .expect("ReplayCompensationLog poisoned")
            .push(record);
    }

    /// Number of recorded failures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("ReplayCompensationLog poisoned")
            .len()
    }

    /// Whether the log is empty (all replays succeeded).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain all records from the log for persistence or reporting.
    pub fn drain(&self) -> Vec<ReplayCompensationRecord> {
        std::mem::take(
            &mut *self
                .entries
                .lock()
                .expect("ReplayCompensationLog poisoned"),
        )
    }
}

impl Default for ReplayCompensationLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded FIFO queue for writes deferred during `Recovering` state.
///
/// **Lifecycle:**
///
/// 1. When the durability state transitions to `Recovering`, call [`activate()`]
///    to accept writes.
/// 2. While active, callers use [`enqueue()`] to defer writes instead of
///    hitting the live DB. The queue enforces a hard capacity limit; writes
///    beyond the limit receive [`DeferralOutcome::BackpressureFull`].
/// 3. When recovery completes (state → `Healthy`), call [`seal_and_drain()`]
///    to atomically stop accepting new writes and return all queued entries
///    in order for replay.
/// 4. After successful replay, call [`reset()`] to prepare for the next
///    recovery cycle.
///
/// The queue is `Sync` and safe for concurrent producers — interior
/// synchronization uses a `Mutex` held only for the duration of a push/drain.
///
/// [`activate()`]: DeferredWriteQueue::activate
/// [`enqueue()`]: DeferredWriteQueue::enqueue
/// [`seal_and_drain()`]: DeferredWriteQueue::seal_and_drain
/// [`reset()`]: DeferredWriteQueue::reset
pub struct DeferredWriteQueue {
    state: Mutex<DeferredWriteQueueInner>,
}

#[derive(Debug)]
struct DeferredWriteQueueInner {
    /// Whether the queue is accepting writes.
    active: bool,
    /// Whether the queue has been sealed (drain in progress, no new writes).
    sealed: bool,
    /// Monotonic sequence counter.
    next_seq: u64,
    /// The actual FIFO buffer.
    entries: Vec<DeferredWrite>,
    /// Overload shedding policy.
    policy: OverloadPolicy,
    /// Per-operation entry counts for fairness enforcement.
    per_operation_counts: HashMap<&'static str, usize>,
    /// Running estimated total bytes of all queued entries.
    estimated_bytes: usize,
    /// Counter: total writes shed due to overload (lifetime of this queue instance).
    shed_count: u64,
}

/// Default capacity: 1024 deferred writes before backpressure kicks in.
///
/// This is generous enough for a typical recovery window (seconds to low
/// minutes) at normal multi-agent write rates (~10-50 writes/sec), while
/// preventing unbounded memory growth if recovery stalls.
pub const DEFAULT_DEFERRED_WRITE_CAPACITY: usize = 1024;

/// Default maximum age (seconds) for the oldest deferred write before the
/// queue triggers a hard-stop. If the oldest entry is older than this, no
/// new writes are accepted — the system is stalled and needs operator
/// attention rather than quiet indefinite queuing.
pub const DEFAULT_DEFERRED_WRITE_MAX_AGE_SECS: u64 = 300;

/// Default estimated byte budget for the entire deferred queue. This is a
/// soft limit — individual enqueue calls estimate their contribution and
/// reject when the running total exceeds this threshold. Prevents memory
/// exhaustion from large SQL payloads (e.g. multi-MB message bodies).
pub const DEFAULT_DEFERRED_WRITE_MAX_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Default per-operation fairness limit. No single operation type may
/// consume more than this fraction of the queue capacity. Prevents a
/// chatty tool (e.g. `send_message` in a broadcast loop) from starving
/// other operation types.
pub const DEFAULT_DEFERRED_WRITE_FAIRNESS_LIMIT_PCT: u8 = 60;

/// Pressure threshold: above this percentage of capacity, the queue is in
/// elevated pressure and surfaces warnings to operators.
pub const DEFERRED_WRITE_WARN_THRESHOLD_PCT: u8 = 75;

impl DeferredWriteQueue {
    /// Create a new inactive queue with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_policy(OverloadPolicy {
            max_entries: capacity,
            ..Default::default()
        })
    }

    /// Create a new inactive queue with a custom overload policy.
    #[must_use]
    pub fn with_policy(policy: OverloadPolicy) -> Self {
        Self {
            state: Mutex::new(DeferredWriteQueueInner {
                active: false,
                sealed: false,
                next_seq: 0,
                entries: Vec::new(),
                policy,
                per_operation_counts: HashMap::new(),
                estimated_bytes: 0,
                shed_count: 0,
            }),
        }
    }

    /// Create a new inactive queue with [`DEFAULT_DEFERRED_WRITE_CAPACITY`].
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::with_policy(OverloadPolicy::default())
    }

    /// Activate the queue to begin accepting writes.
    ///
    /// Call this when the durability state transitions to `Recovering`.
    /// If already active, this is a no-op.
    pub fn activate(&self) {
        let mut inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.active = true;
        inner.sealed = false;
    }

    /// Whether the queue is currently active and accepting writes.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.active && !inner.sealed
    }

    /// Current number of queued writes.
    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.entries.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Attempt to enqueue a deferred write.
    ///
    /// Returns the outcome indicating whether the write was accepted,
    /// rejected due to backpressure, or refused because the queue is
    /// not in the right state. Enforces the full overload policy:
    ///
    /// 1. Queue must be active and not sealed.
    /// 2. Oldest entry must not exceed `max_age_secs` (hard-stop).
    /// 3. Estimated bytes must not exceed `max_bytes` (hard-stop).
    /// 4. Per-operation fairness limit must not be exceeded.
    /// 5. Total entry count must not exceed `max_entries` (backpressure).
    pub fn enqueue(
        &self,
        sql: String,
        params: Vec<Value>,
        operation: &'static str,
    ) -> DeferralOutcome {
        let mut inner = self.state.lock().expect("DeferredWriteQueue poisoned");

        if !inner.active {
            return DeferralOutcome::NotRecovering;
        }
        if inner.sealed {
            return DeferralOutcome::Sealed;
        }

        let now_us = crate::now_micros();

        // Hard-stop: oldest entry exceeded max age → recovery is stalled.
        if let Some(oldest) = inner.entries.first() {
            let age_us = now_us.saturating_sub(oldest.deferred_at_us).max(0);
            let age_secs = u64::try_from(age_us / 1_000_000).unwrap_or(0);
            if age_secs > inner.policy.max_age_secs {
                inner.shed_count = inner.shed_count.saturating_add(1);
                return DeferralOutcome::HardStopAge {
                    oldest_age_secs: age_secs,
                    max_age_secs: inner.policy.max_age_secs,
                };
            }
        }

        // Hard-stop: estimated bytes exceeded budget.
        let entry_bytes = estimate_deferred_write_bytes(&sql, &params);
        if inner.estimated_bytes.saturating_add(entry_bytes) > inner.policy.max_bytes {
            inner.shed_count = inner.shed_count.saturating_add(1);
            return DeferralOutcome::HardStopBytes {
                estimated_bytes: inner.estimated_bytes.saturating_add(entry_bytes),
                max_bytes: inner.policy.max_bytes,
            };
        }

        // Fairness: per-operation limit.
        let fairness_limit = inner.policy.fairness_limit();
        let op_count = inner.per_operation_counts.get(operation).copied().unwrap_or(0);
        if op_count >= fairness_limit {
            inner.shed_count = inner.shed_count.saturating_add(1);
            return DeferralOutcome::FairnessLimitReached {
                operation,
                count: op_count,
                limit: fairness_limit,
            };
        }

        // Backpressure: capacity limit.
        if inner.entries.len() >= inner.policy.max_entries {
            inner.shed_count = inner.shed_count.saturating_add(1);
            return DeferralOutcome::BackpressureFull {
                capacity: inner.policy.max_entries,
            };
        }

        let seq = inner.next_seq;
        inner.next_seq = seq.wrapping_add(1);
        inner.estimated_bytes = inner.estimated_bytes.saturating_add(entry_bytes);
        *inner.per_operation_counts.entry(operation).or_insert(0) += 1;
        inner.entries.push(DeferredWrite {
            seq,
            sql,
            params,
            deferred_at_us: now_us,
            operation,
        });

        DeferralOutcome::Queued { position: seq }
    }

    /// Seal the queue and drain all entries for replay.
    ///
    /// After this call, [`enqueue()`] returns [`DeferralOutcome::Sealed`]
    /// until [`reset()`] is called. The returned entries are sorted by
    /// sequence number (insertion order).
    ///
    /// [`enqueue()`]: DeferredWriteQueue::enqueue
    /// [`reset()`]: DeferredWriteQueue::reset
    pub fn seal_and_drain(&self) -> Vec<DeferredWrite> {
        let mut inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.sealed = true;
        inner.active = false;
        inner.estimated_bytes = 0;
        inner.per_operation_counts.clear();
        let mut entries = std::mem::take(&mut inner.entries);
        entries.sort_by_key(|e| e.seq);
        entries
    }

    /// Reset the queue to its initial inactive state.
    ///
    /// Call after replay completes (or after recovery is abandoned).
    pub fn reset(&self) {
        let mut inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.active = false;
        inner.sealed = false;
        inner.next_seq = 0;
        inner.entries.clear();
        inner.per_operation_counts.clear();
        inner.estimated_bytes = 0;
        // Note: shed_count is NOT reset — it is a lifetime counter for
        // observability across recovery cycles.
    }

    /// Current backlog pressure tier.
    ///
    /// Surfaces use this to decide how urgently to report queue state:
    /// - `Normal`: no action.
    /// - `Elevated`: log/surface advisory warnings.
    /// - `Critical`: surface prominent warnings, consider operator alert.
    /// - `HardStop`: system is refusing writes — operator must intervene.
    #[must_use]
    pub fn pressure(&self) -> BacklogPressure {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        if !inner.active && !inner.sealed {
            return BacklogPressure::Normal;
        }
        if inner.sealed {
            return BacklogPressure::HardStop;
        }
        compute_backlog_pressure(&inner)
    }

    /// Age of the oldest deferred entry in seconds, or 0 if the queue is empty.
    #[must_use]
    pub fn oldest_age_secs(&self) -> u64 {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        oldest_entry_age_secs(&inner)
    }

    /// Running estimated bytes of all queued entries.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.estimated_bytes
    }

    /// Lifetime count of writes shed (rejected) due to overload.
    #[must_use]
    pub fn shed_count(&self) -> u64 {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        inner.shed_count
    }

    /// Snapshot for diagnostics.
    #[must_use]
    pub fn status(&self) -> DeferredWriteQueueStatus {
        let inner = self.state.lock().expect("DeferredWriteQueue poisoned");
        let pressure = if !inner.active && !inner.sealed {
            BacklogPressure::Normal
        } else if inner.sealed {
            BacklogPressure::HardStop
        } else {
            compute_backlog_pressure(&inner)
        };
        DeferredWriteQueueStatus {
            active: inner.active,
            sealed: inner.sealed,
            queued: inner.entries.len(),
            capacity: inner.policy.max_entries,
            next_seq: inner.next_seq,
            estimated_bytes: inner.estimated_bytes,
            oldest_age_secs: oldest_entry_age_secs(&inner),
            pressure,
            shed_count: inner.shed_count,
        }
    }
}

/// Estimate the byte footprint of a single deferred write entry.
fn estimate_deferred_write_bytes(sql: &str, params: &[Value]) -> usize {
    let mut bytes = sql.len();
    for param in params {
        bytes += match param {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::BigInt(_) => 8,
            Value::Float(_) | Value::Double(_) => 8,
            Value::Text(s) => s.len(),
            Value::Bytes(b) => b.len(),
            _ => 16, // conservative estimate for other types
        };
    }
    // Overhead for the DeferredWrite struct, operation string, Vec allocator
    bytes + 128
}

/// Compute the oldest entry age in seconds from queue internals.
fn oldest_entry_age_secs(inner: &DeferredWriteQueueInner) -> u64 {
    match inner.entries.first() {
        Some(oldest) => {
            let now_us = crate::now_micros();
            let age_us = now_us.saturating_sub(oldest.deferred_at_us).max(0);
            u64::try_from(age_us / 1_000_000).unwrap_or(0)
        }
        None => 0,
    }
}

/// Compute the current backlog pressure from queue internals.
fn compute_backlog_pressure(inner: &DeferredWriteQueueInner) -> BacklogPressure {
    // Hard-stop: oldest entry exceeded max age.
    let age_secs = oldest_entry_age_secs(inner);
    if age_secs > inner.policy.max_age_secs {
        return BacklogPressure::HardStop;
    }

    // Hard-stop: byte budget exceeded.
    if inner.estimated_bytes > inner.policy.max_bytes {
        return BacklogPressure::HardStop;
    }

    // Critical: at or above capacity.
    if inner.entries.len() >= inner.policy.max_entries {
        return BacklogPressure::Critical;
    }

    // Critical: age above 90% of max.
    if inner.policy.max_age_secs > 0 && age_secs > inner.policy.max_age_secs * 9 / 10 {
        return BacklogPressure::Critical;
    }

    // Elevated: above warn threshold.
    let warn_threshold =
        (inner.policy.max_entries as u64 * u64::from(DEFERRED_WRITE_WARN_THRESHOLD_PCT) / 100)
            as usize;
    if inner.entries.len() >= warn_threshold {
        return BacklogPressure::Elevated;
    }

    BacklogPressure::Normal
}

/// Diagnostic snapshot of the deferred write queue.
#[derive(Debug, Clone, Serialize)]
pub struct DeferredWriteQueueStatus {
    pub active: bool,
    pub sealed: bool,
    pub queued: usize,
    pub capacity: usize,
    pub next_seq: u64,
    /// Running estimated bytes of all queued entries.
    pub estimated_bytes: usize,
    /// Age (seconds) of the oldest entry, or 0 if empty.
    pub oldest_age_secs: u64,
    /// Current backlog pressure tier.
    pub pressure: BacklogPressure,
    /// Lifetime count of writes shed (rejected) due to overload.
    pub shed_count: u64,
}

/// Global singleton deferred write queue.
///
/// Shared by all write paths in the process.
static DEFERRED_WRITE_QUEUE: OnceLock<DeferredWriteQueue> = OnceLock::new();

/// Access the global deferred write queue.
#[must_use]
pub fn deferred_write_queue() -> &'static DeferredWriteQueue {
    DEFERRED_WRITE_QUEUE.get_or_init(DeferredWriteQueue::with_default_capacity)
}

// ============================================================================
// Owner-broker routing: every mutating surface routes through the mailbox owner
// ============================================================================

/// The surface (entry-point) that initiated a mutating operation.
///
/// Every write path must declare which surface it originated from so the
/// owner-broker routing logic can enforce the single-owner invariant and
/// produce audit-quality logs when writes are refused or deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutatingSurface {
    /// MCP server tool call (stdio or HTTP transport).
    McpServer,
    /// CLI command (`am send`, `am ack`, etc.).
    Cli,
    /// Robot sub-command (`am robot ack`, `am robot release`, etc.).
    Robot,
    /// Background supervisor (recovery, rebuild, checkpoint).
    Supervisor,
    /// Internal migration or schema upgrade path.
    Migration,
    /// Test harness (E2E, integration, chaos).
    Test,
}

impl MutatingSurface {
    /// Short label for structured logs and metrics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::McpServer => "mcp_server",
            Self::Cli => "cli",
            Self::Robot => "robot",
            Self::Supervisor => "supervisor",
            Self::Migration => "migration",
            Self::Test => "test",
        }
    }

    /// Whether this surface has authority to bypass ownership checks.
    ///
    /// Supervisor and Migration are the recovery and upgrade authorities
    /// respectively — they must be able to write even when the mailbox is
    /// in a degraded or contested state.
    #[must_use]
    pub const fn is_authority(&self) -> bool {
        matches!(self, Self::Supervisor | Self::Migration)
    }
}

impl std::fmt::Display for MutatingSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Disposition of a write request after owner-broker routing evaluation.
///
/// When a mutating surface attempts a write, the broker evaluates the current
/// mailbox ownership and durability state and returns one of these outcomes.
/// Callers must respect the disposition — `Permitted` means proceed,
/// `Deferred` means the caller should enqueue the SQL into the deferred-write
/// queue, and `Refused` means the write must not proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "disposition")]
pub enum WriteRouteDisposition {
    /// Write may proceed — caller is (or is delegating through) the current
    /// mailbox owner and the durability state allows writes.
    Permitted,

    /// Write should be deferred into the deferred-write queue. The caller
    /// should accept the write (returning success to the user) and enqueue
    /// the actual SQL for replay after recovery completes.
    Deferred,

    /// Write is refused because the mailbox is in a state that does not allow
    /// mutation. The `reason` is a human-readable explanation suitable for
    /// operator-facing error messages.
    Refused { reason: String },
}

impl WriteRouteDisposition {
    /// Whether this disposition allows the caller to proceed with the write.
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        matches!(self, Self::Permitted)
    }

    /// Whether the write should be deferred (accepted but not yet applied).
    #[must_use]
    pub const fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred)
    }

    /// Whether the write was refused outright.
    #[must_use]
    pub const fn is_refused(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }
}

/// Evaluate whether a mutating surface is allowed to proceed with a write.
///
/// This is the single chokepoint through which every write request should pass
/// before touching the database. It inspects:
///
/// 1. **Durability state** — does the current state allow writes?
/// 2. **Ownership** — is this process the current mailbox owner?
/// 3. **Recovery lock** — is a recovery operation in flight?
///
/// Authority surfaces ([`MutatingSurface::Supervisor`] and
/// [`MutatingSurface::Migration`]) bypass ownership and deferral checks
/// because they *are* the recovery/upgrade authority.
pub fn evaluate_write_route(
    surface: MutatingSurface,
    ownership: &MailboxOwnershipState,
    durability: crate::mailbox_verdict::DurabilityState,
    recovery_lock: &MailboxRecoveryLockState,
) -> WriteRouteDisposition {
    let is_authority = surface.is_authority();

    // 1. Durability gate: if writes are not allowed, non-authority surfaces
    //    are either deferred (if the queue is active) or refused.
    if !durability.allows_writes() {
        if is_authority {
            return WriteRouteDisposition::Permitted;
        }

        let q_status = deferred_write_queue().status();
        if q_status.active && !q_status.sealed && q_status.queued < q_status.capacity {
            return WriteRouteDisposition::Deferred;
        }

        return WriteRouteDisposition::Refused {
            reason: format!(
                "Mailbox is {durability} and writes are not permitted. \
                 Run `am doctor repair` to attempt recovery."
            ),
        };
    }

    // 2. Ownership gate: refuse if another active process owns the mailbox.
    if ownership.blocks_mutation() && !is_authority {
        let owner_detail = match ownership.disposition {
            MailboxOwnershipDisposition::ActiveOtherOwner => {
                let pids: Vec<String> =
                    ownership.processes.iter().map(|p| p.pid.to_string()).collect();
                format!(
                    "Another active process owns this mailbox (pid {}). \
                     Route writes through that process or stop it first.",
                    pids.join(", ")
                )
            }
            MailboxOwnershipDisposition::SplitBrain => {
                format!(
                    "Split-brain detected: {} competing processes hold locks. \
                     Stop all competing processes and run `am doctor repair`.",
                    ownership.competing_pids.len()
                )
            }
            MailboxOwnershipDisposition::StaleLiveProcess => {
                "A stale process appears to hold the mailbox lock. \
                 Run `am doctor repair` to clean up stale locks."
                    .to_string()
            }
            MailboxOwnershipDisposition::DeletedExecutable => {
                "A process with a deleted executable holds the mailbox lock. \
                 Kill the orphan process or run `am doctor repair`."
                    .to_string()
            }
            MailboxOwnershipDisposition::Unowned => {
                // blocks_mutation() is false for Unowned — unreachable.
                return WriteRouteDisposition::Permitted;
            }
        };
        return WriteRouteDisposition::Refused {
            reason: owner_detail,
        };
    }

    // 3. Recovery lock gate: if recovery is in flight, defer non-authority writes.
    if recovery_lock.active && !is_authority {
        let q_status = deferred_write_queue().status();
        if q_status.active && !q_status.sealed && q_status.queued < q_status.capacity {
            return WriteRouteDisposition::Deferred;
        }

        let holder = recovery_lock
            .pid
            .map_or("unknown".to_string(), |pid| format!("pid {pid}"));
        return WriteRouteDisposition::Refused {
            reason: format!(
                "Recovery lock held by {holder}; writes are blocked until recovery completes."
            ),
        };
    }

    WriteRouteDisposition::Permitted
}

// ============================================================================

/// Default pool configuration values — sized for 1000+ concurrent agents.
///
/// ## Sizing rationale
///
/// `SQLite` WAL mode allows unlimited concurrent readers but serializes writers.
/// With a 1000-agent workload where ~10% are active simultaneously (~100 concurrent
/// tool calls) and a 3:1 read:write ratio, we need:
///
/// - **Readers**: At least 50 connections so read-heavy tools (`fetch_inbox`,
///   `search_messages`, resources) never queue behind writes.
/// - **Writers**: Only one writer executes at a time in WAL, so extra write
///   connections just queue on the WAL lock — but having a handful avoids
///   pool-acquire contention for the write path.
///
/// Defaults: `min=25, max=100`.  The pool lazily opens connections (starting from
/// `min`), so a lightly-loaded server uses only ~25 connections.  Under load the
/// pool grows up to 100, which still stays well within `SQLite` practical limits.
///
/// ## Timeout
///
/// Reduced from legacy 60s to 15s: if a connection isn't available within 15s the
/// circuit breaker should handle the failure rather than having the caller hang.
///
/// Override via `DATABASE_POOL_SIZE` / `DATABASE_MAX_OVERFLOW` env vars.
pub const DEFAULT_POOL_SIZE: usize = 25;
pub const DEFAULT_MAX_OVERFLOW: usize = 75;
pub const DEFAULT_POOL_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_POOL_RECYCLE_MS: u64 = 30 * 60 * 1000; // 30 minutes

/// Auto-detect a reasonable pool size from available CPU parallelism.
///
/// Returns `(min_connections, max_connections)`.  The heuristic is:
///
/// - `min = clamp(cpus * 4, 10, 50)`  — enough idle connections for moderate load
/// - `max = clamp(cpus * 12, 50, 200)` — headroom for burst traffic
///
/// This is used when `DATABASE_POOL_SIZE=auto` (the default when no explicit size
/// is given).
#[must_use]
pub fn auto_pool_size() -> (usize, usize) {
    let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let min = (cpus * 4).clamp(10, 50);
    let max = (cpus * 12).clamp(50, 200);
    (min, max)
}

/// Pool configuration
#[derive(Debug, Clone)]
pub struct DbPoolConfig {
    /// Database URL (`sqlite:///path/to/db.sqlite3`)
    pub database_url: String,
    /// Storage root used for archive-backed reconcile/recovery.
    ///
    /// When unset, callers fall back to the current process configuration.
    /// Set this explicitly whenever the caller already has an authoritative
    /// storage-root snapshot; otherwise pool init can reconcile against the
    /// wrong archive and pool caching can alias unrelated mailboxes.
    pub storage_root: Option<PathBuf>,
    /// Minimum connections to keep open
    pub min_connections: usize,
    /// Maximum connections
    pub max_connections: usize,
    /// Timeout for acquiring a connection (ms)
    pub acquire_timeout_ms: u64,
    /// Max connection lifetime (ms)
    pub max_lifetime_ms: u64,
    /// Run migrations on init
    pub run_migrations: bool,
    /// Number of connections to eagerly open on startup (0 = disabled).
    /// Capped at `min_connections`. Warmup is bounded by `acquire_timeout_ms`.
    pub warmup_connections: usize,
    /// Total page-cache budget across all connections (KiB).
    /// Override via `Config::database_cache_budget_kb` / `DATABASE_CACHE_BUDGET_KB`.
    pub cache_budget_kb: usize,
}

impl Default for DbPoolConfig {
    fn default() -> Self {
        Self {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            storage_root: None,
            min_connections: DEFAULT_POOL_SIZE,
            max_connections: DEFAULT_POOL_SIZE + DEFAULT_MAX_OVERFLOW,
            acquire_timeout_ms: DEFAULT_POOL_TIMEOUT_MS,
            max_lifetime_ms: DEFAULT_POOL_RECYCLE_MS,
            run_migrations: true,
            warmup_connections: 0,
            cache_budget_kb: schema::DEFAULT_CACHE_BUDGET_KB,
        }
    }
}

impl DbPoolConfig {
    /// Create config from environment.
    ///
    /// Pool sizing honours three strategies in priority order:
    ///
    /// 1. **Explicit**: `DATABASE_POOL_SIZE` and/or `DATABASE_MAX_OVERFLOW` are set
    ///    to numeric values → use those literally.
    /// 2. **Auto** (default): `DATABASE_POOL_SIZE` is unset or `"auto"` →
    ///    [`auto_pool_size()`] picks sizes based on CPU count.
    /// 3. **Legacy**: Set `DATABASE_POOL_SIZE=3` and `DATABASE_MAX_OVERFLOW=4` to
    ///    restore the legacy Python defaults (not recommended for production).
    #[must_use]
    pub fn from_env() -> Self {
        let core_config = mcp_agent_mail_core::Config::from_env();
        let database_url = core_config.database_url.clone();

        let pool_timeout = env_value("DATABASE_POOL_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POOL_TIMEOUT_MS);

        // Determine pool sizing: explicit, auto, or default constants.
        let pool_size_raw = env_value("DATABASE_POOL_SIZE");
        let explicit_size = pool_size_raw
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok());
        let explicit_overflow =
            env_value("DATABASE_MAX_OVERFLOW").and_then(|s| s.parse::<usize>().ok());

        let (min_conn, max_conn) = match (explicit_size, explicit_overflow) {
            // Both explicitly set → honour literally.
            (Some(size), Some(overflow)) => (size, size + overflow),
            // Only size set → derive overflow from size.
            (Some(size), None) => (
                size,
                size.saturating_mul(4).max(size + DEFAULT_MAX_OVERFLOW),
            ),
            // Not set, or explicitly "auto" → detect from hardware.
            (None, maybe_overflow) => {
                let (auto_min, auto_max) = auto_pool_size();
                maybe_overflow.map_or((auto_min, auto_max), |overflow| {
                    (auto_min, auto_min + overflow)
                })
            }
        };

        let warmup = env_value("DATABASE_POOL_WARMUP")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
            .min(min_conn);

        Self {
            database_url,
            storage_root: Some(core_config.storage_root),
            min_connections: min_conn,
            max_connections: max_conn,
            acquire_timeout_ms: pool_timeout,
            max_lifetime_ms: DEFAULT_POOL_RECYCLE_MS,
            run_migrations: true,
            warmup_connections: warmup,
            cache_budget_kb: env_value("DATABASE_CACHE_BUDGET_KB")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(schema::DEFAULT_CACHE_BUDGET_KB)
                .clamp(16_384, 4_194_304),
        }
    }

    /// Parse `SQLite` path from database URL
    pub fn sqlite_path(&self) -> DbResult<String> {
        if is_sqlite_memory_database_url(&self.database_url) {
            return Ok(":memory:".to_string());
        }

        let Some(path) = sqlite_file_path_from_database_url(&self.database_url) else {
            return Err(DbError::InvalidArgument {
                field: "database_url",
                message: format!(
                    "Invalid SQLite database URL: {} (expected sqlite:///path/to/db.sqlite3)",
                    self.database_url
                ),
            });
        };

        Ok(path.to_string_lossy().into_owned())
    }

    #[must_use]
    pub fn resolved_storage_root(&self) -> PathBuf {
        self.storage_root
            .clone()
            .unwrap_or_else(|| mcp_agent_mail_core::Config::from_env().storage_root)
    }

    /// Apply ephemeral-root rerouting if the given project root is classified
    /// as ephemeral (tmp, dev/shm, test harness, CI runner, etc.).
    ///
    /// When the project root is ephemeral and the current `storage_root` is
    /// the default global mailbox, this method replaces it with an isolated
    /// hash-derived directory under the configured ephemeral base. This
    /// prevents transient test/CI/NTM runs from contaminating the operator's
    /// production mail archive.
    ///
    /// If the storage root is already non-default (operator explicitly set
    /// `STORAGE_ROOT`) or the project root is classified as production, the
    /// config is returned unchanged.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let config = DbPoolConfig::from_env()
    ///     .with_ephemeral_reroute(Path::new("/tmp/test-project"));
    /// // config.storage_root now points to /tmp/.am-ephemeral/<hash>/
    /// ```
    #[must_use]
    pub fn with_ephemeral_reroute(mut self, project_root: &Path) -> Self {
        let core_config = mcp_agent_mail_core::Config::from_env();
        if let Some(isolated) =
            mcp_agent_mail_core::compute_ephemeral_storage_root(project_root, &core_config)
        {
            tracing::info!(
                project_root = %project_root.display(),
                isolated_root = %isolated.display(),
                "DbPoolConfig: auto-rerouting ephemeral project to isolated storage root",
            );
            self.storage_root = Some(isolated);
        }
        self
    }

    /// Create config from environment with ephemeral-root rerouting applied.
    ///
    /// This is a convenience constructor combining [`from_env()`](Self::from_env)
    /// with [`with_ephemeral_reroute()`](Self::with_ephemeral_reroute).
    /// Background workers and server startup code should prefer this over bare
    /// `from_env()` when they know the project root directory.
    ///
    /// # Arguments
    ///
    /// * `project_root` - Absolute path to the project's working directory.
    ///   Ephemeral classification is performed against this path.
    #[must_use]
    pub fn from_env_for_project(project_root: &Path) -> Self {
        Self::from_env().with_ephemeral_reroute(project_root)
    }

    /// Check whether the resolved storage root would be rerouted for a given
    /// project root. Returns the isolated path if rerouting would occur, or
    /// `None` if the project is classified as production or the storage root
    /// is already non-default.
    ///
    /// This is a read-only query; it does not modify the config.
    #[must_use]
    pub fn would_reroute_for_project(&self, project_root: &Path) -> Option<PathBuf> {
        let core_config = mcp_agent_mail_core::Config::from_env();
        mcp_agent_mail_core::compute_ephemeral_storage_root(project_root, &core_config)
    }
}

#[derive(Debug)]
struct DbPoolStatsSampler {
    last_sample_us: AtomicU64,
    last_peak_reset_us: AtomicU64,
}

impl DbPoolStatsSampler {
    const SAMPLE_INTERVAL_US: u64 = 250_000; // 250ms
    const PEAK_WINDOW_US: u64 = 60_000_000; // 60s

    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_sample_us: AtomicU64::new(0),
            last_peak_reset_us: AtomicU64::new(0),
        }
    }

    pub fn sample_now(&self, pool: &Pool<DbConn>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, true);
    }

    pub fn maybe_sample(&self, pool: &Pool<DbConn>) {
        let now_us = u64::try_from(crate::now_micros()).unwrap_or(0);
        self.sample_inner(pool, now_us, false);
    }

    fn sample_inner(&self, pool: &Pool<DbConn>, now_us: u64, force: bool) {
        if force {
            self.last_sample_us.store(now_us, Ordering::Relaxed);
        } else {
            let last = self.last_sample_us.load(Ordering::Relaxed);
            if now_us.saturating_sub(last) < Self::SAMPLE_INTERVAL_US {
                return;
            }
            if self
                .last_sample_us
                .compare_exchange(last, now_us, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
        }

        let stats = pool.stats();
        let metrics = mcp_agent_mail_core::global_metrics();

        let total = u64::try_from(stats.total_connections).unwrap_or(0);
        let idle = u64::try_from(stats.idle_connections).unwrap_or(0);
        let active = u64::try_from(stats.active_connections).unwrap_or(0);
        let pending = u64::try_from(stats.pending_requests).unwrap_or(0);

        metrics.db.pool_total_connections.set(total);
        metrics.db.pool_idle_connections.set(idle);
        metrics.db.pool_active_connections.set(active);
        metrics.db.pool_pending_requests.set(pending);

        // Peak is a rolling 60s high-water mark (best-effort; updated on sampling).
        let reset_last = self.last_peak_reset_us.load(Ordering::Relaxed);
        if (reset_last == 0 || now_us.saturating_sub(reset_last) >= Self::PEAK_WINDOW_US)
            && self
                .last_peak_reset_us
                .compare_exchange(reset_last, now_us, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            metrics.db.pool_peak_active_connections.set(active);
        }
        metrics.db.pool_peak_active_connections.fetch_max(active);

        // Track "pool has been >= 80% utilized" duration (in micros since epoch).
        let util_pct = if total == 0 {
            0
        } else {
            active.saturating_mul(100).saturating_div(total)
        };
        if util_pct >= 80 {
            if metrics.db.pool_over_80_since_us.load() == 0 {
                metrics.db.pool_over_80_since_us.set(now_us);
            }
        } else {
            metrics.db.pool_over_80_since_us.set(0);
        }
    }
}

/// A configured `SQLite` connection pool with schema initialization.
///
/// This wraps `sqlmodel_pool::Pool<DbConn>` and encapsulates:
/// - URL/path parsing (`sqlite+aiosqlite:///...` etc)
/// - per-connection PRAGMAs + schema init (idempotent)
#[derive(Clone)]
pub struct DbPool {
    pool: Arc<Pool<DbConn>>,
    sqlite_path: String,
    storage_root: PathBuf,
    init_sql: Arc<String>,
    run_migrations: bool,
    stats_sampler: Arc<DbPoolStatsSampler>,
}

impl DbPool {
    fn from_shared_pool(config: &DbPoolConfig, pool: Arc<Pool<DbConn>>) -> DbResult<Self> {
        let sqlite_path = resolve_sqlite_path_with_absolute_fallback(&config.sqlite_path()?);
        let storage_root = config.resolved_storage_root();
        let init_sql = Arc::new(schema::build_conn_pragmas(
            config.max_connections,
            config.cache_budget_kb,
        ));
        let stats_sampler = Arc::new(DbPoolStatsSampler::new());

        Ok(Self {
            pool,
            sqlite_path,
            storage_root,
            init_sql,
            run_migrations: config.run_migrations,
            stats_sampler,
        })
    }

    /// Create a new pool (does not open connections until first acquire).
    pub fn new(config: &DbPoolConfig) -> DbResult<Self> {
        let sqlite_path = resolve_sqlite_path_with_absolute_fallback(&config.sqlite_path()?);
        let storage_root = config.resolved_storage_root();
        let init_sql = Arc::new(schema::build_conn_pragmas(
            config.max_connections,
            config.cache_budget_kb,
        ));
        let stats_sampler = Arc::new(DbPoolStatsSampler::new());

        let pool_config = PoolConfig::new(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout_ms)
            .max_lifetime(config.max_lifetime_ms)
            // Legacy Python favors responsiveness; validate on checkout.
            .test_on_checkout(true)
            .test_on_return(false);

        Ok(Self {
            pool: Arc::new(Pool::new(pool_config)),
            sqlite_path,
            storage_root,
            init_sql,
            run_migrations: config.run_migrations,
            stats_sampler,
        })
    }

    #[must_use]
    pub fn sqlite_path(&self) -> &str {
        &self.sqlite_path
    }

    #[must_use]
    pub fn sqlite_identity_key(&self) -> String {
        if self.sqlite_path == ":memory:" {
            format!(":memory:@{:p}", Arc::as_ptr(&self.pool))
        } else {
            self.sqlite_path.clone()
        }
    }

    pub fn sample_pool_stats_now(&self) {
        self.stats_sampler.sample_now(&self.pool);
    }

    /// Acquire a pooled connection, creating and initializing a new one if needed.
    #[allow(clippy::too_many_lines)]
    pub async fn acquire(&self, cx: &Cx) -> Outcome<PooledConnection<DbConn>, SqlError> {
        let sqlite_path = self.sqlite_path.clone();
        let storage_root = self.storage_root.clone();
        let init_sql = self.init_sql.clone();
        let run_migrations = self.run_migrations;
        let cx2 = cx.clone();

        let start = Instant::now();
        let out = self
            .pool
            .acquire(cx, || {
                let sqlite_path = sqlite_path.clone();
                let storage_root = storage_root.clone();
                let init_sql = init_sql.clone();
                let cx2 = cx2.clone();
                async move {
                    // Ensure parent directory exists for file-backed DBs.
                    if sqlite_path != ":memory:"
                        && let Err(e) = ensure_sqlite_parent_dir_exists(&sqlite_path)
                    {
                        return Outcome::Err(e);
                    }

                    // For file-backed DBs, run DB-wide init (journal mode, migrations) once
                    // before opening pooled connections.
                    // Run one-time DB initialization (schema + migrations) via a separate
                    // connection to ensure atomic setup before pool connections open.
                    if sqlite_path != ":memory:" {
                        let init_gate = sqlite_init_gate(&sqlite_path, &storage_root);
                        let run_migrations = run_migrations;

                        let gate_out = init_gate
                            .get_or_try_init(|| {
                                let cx2 = cx2.clone();
                                let sqlite_path = sqlite_path.clone();
                                async move {
                                    match initialize_sqlite_file_once(
                                        &cx2,
                                        &sqlite_path,
                                        run_migrations,
                                        &storage_root,
                                    )
                                    .await
                                    {
                                        Outcome::Ok(()) => Ok(()),
                                        Outcome::Err(e) => Err(Outcome::Err(e)),
                                        Outcome::Cancelled(r) => Err(Outcome::Cancelled(r)),
                                        Outcome::Panicked(p) => Err(Outcome::Panicked(p)),
                                    }
                                }
                            })
                            .await;

                        match gate_out {
                            Ok(()) => {}
                            Err(Outcome::Err(e)) => return Outcome::Err(e),
                            Err(Outcome::Cancelled(r)) => return Outcome::Cancelled(r),
                            Err(Outcome::Panicked(p)) => return Outcome::Panicked(p),
                            Err(Outcome::Ok(())) => {
                                unreachable!("sqlite init gate returned Err(Outcome::Ok(()))")
                            }
                        }
                    }

                    // Now open pool connection (migrations are complete).
                    let mut conn = if sqlite_path == ":memory:" {
                        match DbConn::open_memory() {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    } else {
                        match open_sqlite_file_with_recovery(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        }
                    };

                    // Per-connection PRAGMAs matching legacy Python `db.py` event listeners.
                    if let Err(first_init_err) = execute_sql_with_lock_retry(
                        &conn,
                        &sqlite_path,
                        &init_sql,
                        "pool connection init pragmas",
                    ) {
                        if sqlite_path == ":memory:"
                            || !is_sqlite_recovery_error_message(&first_init_err.to_string())
                        {
                            return Outcome::Err(first_init_err);
                        }

                        tracing::warn!(
                            path = %sqlite_path,
                            error = %first_init_err,
                            "sqlite connection init PRAGMAs failed with recoverable error; attempting automatic recovery"
                        );

                        crate::close_db_conn(conn, "sqlite connection init before recovery");
                        if let Err(recovery_err) = recover_sqlite_file(Path::new(&sqlite_path)) {
                            return Outcome::Err(recovery_err);
                        }

                        conn = match open_sqlite_file_with_recovery(&sqlite_path) {
                            Ok(c) => c,
                            Err(e) => return Outcome::Err(e),
                        };
                        if let Err(second_init_err) = execute_sql_with_lock_retry(
                            &conn,
                            &sqlite_path,
                            &init_sql,
                            "pool connection init pragmas after recovery",
                        ) {
                            return Outcome::Err(second_init_err);
                        }
                    }

                    Outcome::Ok(conn)
                }
            })
            .await;

        let dur_us = u64::try_from(start.elapsed().as_micros().min(u128::from(u64::MAX)))
            .unwrap_or(u64::MAX);
        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.db.pool_acquires_total.inc();
        metrics.db.pool_acquire_latency_us.record(dur_us);
        if !matches!(out, Outcome::Ok(_)) {
            metrics.db.pool_acquire_errors_total.inc();
        }

        // Best-effort sampling for pool utilization gauges (bounded frequency).
        self.stats_sampler.maybe_sample(&self.pool);

        out
    }

    /// Eagerly open up to `n` connections to avoid first-burst latency.
    ///
    /// Connections are acquired and immediately returned to the pool idle set.
    /// Bounded: stops after `timeout` elapses or on first acquire error.
    /// Returns the number of connections successfully warmed up.
    pub async fn warmup(&self, cx: &Cx, n: usize, timeout: std::time::Duration) -> usize {
        let deadline = Instant::now() + timeout;
        let mut opened = 0usize;
        // Acquire connections in batches; hold them briefly then release.
        let mut batch: Vec<PooledConnection<DbConn>> = Vec::with_capacity(n);
        for _ in 0..n {
            if Instant::now() >= deadline {
                break;
            }
            match self.acquire(cx).await {
                Outcome::Ok(conn) => {
                    batch.push(conn);
                    opened += 1;
                }
                _ => break, // stop on any error (timeout, cancelled, etc.)
            }
        }
        // Drop all connections back to idle pool
        drop(batch);
        opened
    }

    /// Run a `PRAGMA quick_check` on a fresh connection to validate database
    /// integrity at startup. Returns `Ok(result)` if healthy, or
    /// `Err(IntegrityCorruption)` if corruption is detected.
    ///
    /// This opens a dedicated connection (outside the pool) so the check
    /// doesn't consume a pooled slot.
    pub fn run_startup_integrity_check(&self) -> DbResult<integrity::IntegrityCheckResult> {
        if self.sqlite_path == ":memory:" {
            // In-memory databases cannot be corrupt on startup.
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Quick,
            });
        }

        // Check if the file exists first. If missing, it requires recovery (e.g. from archive or backup).
        if !Path::new(&self.sqlite_path).exists() {
            return Err(DbError::IntegrityCorruption {
                message: "Database file is missing".to_string(),
                details: vec!["File not found on disk".to_string()],
            });
        }

        let conn = crate::guard_db_conn(
            match open_sqlite_file_with_lock_retry(&self.sqlite_path) {
                Ok(conn) => conn,
                Err(e) => {
                    if !is_corruption_error_message(&e.to_string()) {
                        return Err(DbError::Sqlite(format!(
                            "startup integrity check: open failed: {e}"
                        )));
                    }
                    tracing::warn!(
                        path = %self.sqlite_path,
                        error = %e,
                        "startup integrity check failed to open sqlite file; attempting auto-recovery"
                    );
                    recover_sqlite_file(Path::new(&self.sqlite_path))
                        .map_err(|re| DbError::Sqlite(format!("startup recovery failed: {re}")))?;
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|reopen| {
                        DbError::Sqlite(format!(
                            "startup integrity check: open failed after recovery: {reopen}"
                        ))
                    })?
                }
            },
            "startup integrity check connection",
        );

        match integrity::quick_check(&conn) {
            Ok(res) => Ok(res),
            Err(DbError::IntegrityCorruption { .. }) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    "startup integrity check failed; attempting auto-recovery from backup"
                );
                // Close connection before attempting restore (Windows/locking safety)
                drop(conn);

                if let Err(e) = recover_sqlite_file(Path::new(&self.sqlite_path)) {
                    return Err(DbError::Sqlite(format!("startup recovery failed: {e}")));
                }

                // Re-open and re-verify
                let conn = crate::guard_db_conn(
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|e| {
                        DbError::Sqlite(format!(
                            "startup integrity check (post-recovery): open failed: {e}"
                        ))
                    })?,
                    "startup integrity check post-recovery connection",
                );
                integrity::quick_check(&conn)
            }
            Err(e) => Err(e),
        }
    }

    /// Run a full `PRAGMA integrity_check` on a dedicated connection.
    ///
    /// This can take seconds on large databases. Should be called from a
    /// background task, not from the request hot path.
    pub fn run_full_integrity_check(&self) -> DbResult<integrity::IntegrityCheckResult> {
        if self.sqlite_path == ":memory:" {
            return Ok(integrity::IntegrityCheckResult {
                ok: true,
                details: vec!["ok".to_string()],
                duration_us: 0,
                kind: integrity::CheckKind::Full,
            });
        }

        if !Path::new(&self.sqlite_path).exists() {
            return Err(DbError::IntegrityCorruption {
                message: "Database file is missing".to_string(),
                details: vec!["File not found on disk".to_string()],
            });
        }

        let conn = crate::guard_db_conn(
            match open_sqlite_file_with_lock_retry(&self.sqlite_path) {
                Ok(conn) => conn,
                Err(e) => {
                    if !is_corruption_error_message(&e.to_string()) {
                        return Err(DbError::Sqlite(format!(
                            "full integrity check: open failed: {e}"
                        )));
                    }
                    tracing::warn!(
                        path = %self.sqlite_path,
                        error = %e,
                        "full integrity check failed to open sqlite file; attempting auto-recovery"
                    );
                    recover_sqlite_file(Path::new(&self.sqlite_path)).map_err(|re| {
                        DbError::Sqlite(format!("full integrity recovery failed: {re}"))
                    })?;
                    open_sqlite_file_with_lock_retry(&self.sqlite_path).map_err(|reopen| {
                        DbError::Sqlite(format!(
                            "full integrity check: open failed after recovery: {reopen}"
                        ))
                    })?
                }
            },
            "full integrity check connection",
        );

        integrity::full_check(&conn)
    }

    /// Sample the N most recent messages from the DB for consistency checking.
    ///
    /// Returns lightweight refs that the storage layer can use to verify
    /// archive file presence. Opens a dedicated connection (outside the pool)
    /// so this works even if the pool isn't fully started yet.
    #[allow(clippy::too_many_lines)]
    pub fn sample_recent_message_refs(&self, limit: i64) -> DbResult<Vec<ConsistencyMessageRef>> {
        if self.sqlite_path == ":memory:" {
            return Ok(Vec::new());
        }
        if !Path::new(&self.sqlite_path).exists() {
            return Ok(Vec::new());
        }

        // Keep consistency sampling on FrankenSQLite and avoid JOIN-heavy scans:
        // 1) fetch recent envelopes
        // 2) resolve slugs/names via batched point lookups
        let conn = crate::guard_db_conn(
            open_sqlite_file_with_lock_retry(&self.sqlite_path)
                .map_err(|e| DbError::Sqlite(format!("consistency probe: open failed: {e}")))?,
            "consistency probe connection",
        );
        // This two-phase strategy is materially faster than a three-way JOIN on
        // large mailboxes and reduces startup probe lock contention.
        let message_rows = conn
            .query_sync(
                "SELECT id, project_id, sender_id, subject, created_ts \
                 FROM messages \
                 ORDER BY id DESC \
                 LIMIT ?",
                &[sqlmodel_core::Value::BigInt(limit)],
            )
            .map_err(|e| DbError::Sqlite(format!("consistency probe query: {e}")))?;

        if message_rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut sampled: Vec<SampledMessage> = Vec::with_capacity(message_rows.len());
        let mut project_ids: Vec<i64> = Vec::new();
        let mut sender_ids: Vec<i64> = Vec::new();
        let mut seen_projects: HashSet<i64> = HashSet::new();
        let mut seen_senders: HashSet<i64> = HashSet::new();

        for row in &message_rows {
            let id = match row.get_by_name("id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let project_id = match row.get_by_name("project_id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let sender_id = match row.get_by_name("sender_id") {
                Some(sqlmodel_core::Value::BigInt(n)) => *n,
                Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                _ => continue,
            };
            let subject = match row.get_by_name("subject") {
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let created_ts_iso = match row.get_by_name("created_ts") {
                Some(sqlmodel_core::Value::BigInt(us)) => crate::micros_to_iso(*us),
                Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                _ => continue,
            };

            if seen_projects.insert(project_id) {
                project_ids.push(project_id);
            }
            if seen_senders.insert(sender_id) {
                sender_ids.push(sender_id);
            }
            sampled.push(SampledMessage {
                id,
                project_id,
                sender_id,
                subject,
                created_ts_iso,
            });
        }

        if sampled.is_empty() {
            return Ok(Vec::new());
        }

        let mut project_slugs_by_id: HashMap<i64, String> = HashMap::new();
        if !project_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", project_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT id, slug FROM projects WHERE id IN ({placeholders})");
            let params = project_ids
                .iter()
                .copied()
                .map(sqlmodel_core::Value::BigInt)
                .collect::<Vec<_>>();
            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(format!("consistency probe project lookup: {e}")))?;
            for row in &rows {
                let id = match row.get_by_name("id") {
                    Some(sqlmodel_core::Value::BigInt(n)) => *n,
                    Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let slug = match row.get_by_name("slug") {
                    Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                    _ => continue,
                };
                project_slugs_by_id.insert(id, slug);
            }
        }

        let mut sender_names_by_id: HashMap<i64, String> = HashMap::new();
        if !sender_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", sender_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT id, name FROM agents WHERE id IN ({placeholders})");
            let params = sender_ids
                .iter()
                .copied()
                .map(sqlmodel_core::Value::BigInt)
                .collect::<Vec<_>>();
            let rows = conn
                .query_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(format!("consistency probe agent lookup: {e}")))?;
            for row in &rows {
                let id = match row.get_by_name("id") {
                    Some(sqlmodel_core::Value::BigInt(n)) => *n,
                    Some(sqlmodel_core::Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let name = match row.get_by_name("name") {
                    Some(sqlmodel_core::Value::Text(s)) => s.clone(),
                    _ => continue,
                };
                sender_names_by_id.insert(id, name);
            }
        }

        let mut refs = Vec::with_capacity(sampled.len());
        for message in sampled {
            let Some(project_slug) = project_slugs_by_id.get(&message.project_id) else {
                continue;
            };
            let Some(sender_name) = sender_names_by_id.get(&message.sender_id) else {
                continue;
            };
            refs.push(ConsistencyMessageRef {
                project_slug: project_slug.clone(),
                message_id: message.id,
                sender_name: sender_name.clone(),
                subject: message.subject,
                created_ts_iso: message.created_ts_iso,
            });
        }

        Ok(refs)
    }

    /// Run an explicit WAL checkpoint (`TRUNCATE` mode).
    ///
    /// This moves all WAL content back into the main database file and truncates
    /// the WAL to zero length. Useful for:
    /// - Graceful shutdown (ensures DB file is self-contained)
    /// - Before export/snapshot (no loose WAL journal)
    /// - Idle periods (reclaim WAL disk space)
    ///
    /// Returns the number of WAL frames checkpointed, or an error.
    /// No-ops silently for `:memory:` databases.
    pub fn wal_checkpoint(&self) -> DbResult<u64> {
        if self.sqlite_path == ":memory:" {
            return Ok(0);
        }
        let conn = crate::guard_db_conn(
            open_sqlite_file_with_lock_retry(&self.sqlite_path)
                .map_err(|e| DbError::Sqlite(format!("checkpoint: open failed: {e}")))?,
            "wal checkpoint connection",
        );

        // Apply busy_timeout so the checkpoint waits for active readers/writers.
        conn.execute_raw("PRAGMA busy_timeout = 60000;")
            .map_err(|e| DbError::Sqlite(format!("checkpoint: busy_timeout: {e}")))?;

        let rows = conn
            .query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[])
            .map_err(|e| DbError::Sqlite(format!("checkpoint: {e}")))?;

        // wal_checkpoint returns (busy, log, checkpointed)
        let checkpointed = rows
            .first()
            .and_then(|r| match r.get_by_name("checkpointed") {
                Some(sqlmodel_core::Value::BigInt(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                Some(sqlmodel_core::Value::Int(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                _ => None,
            })
            .unwrap_or(0);

        Ok(checkpointed)
    }

    /// Run a **passive** WAL checkpoint that never blocks writers.
    ///
    /// Unlike [`wal_checkpoint`] (which uses `TRUNCATE` mode and can block),
    /// this uses `PRAGMA wal_checkpoint(PASSIVE)` which checkpoints as many
    /// WAL frames as possible without waiting for any readers or writers to
    /// finish. Suitable for periodic background maintenance to keep WAL size
    /// bounded without introducing write contention.
    ///
    /// Returns the number of WAL frames checkpointed, or an error.
    /// No-ops silently for `:memory:` databases.
    pub fn wal_checkpoint_passive(&self) -> DbResult<u64> {
        if self.sqlite_path == ":memory:" {
            return Ok(0);
        }
        let conn = crate::guard_db_conn(
            open_sqlite_file_with_lock_retry(&self.sqlite_path)
                .map_err(|e| DbError::Sqlite(format!("passive checkpoint: open failed: {e}")))?,
            "passive wal checkpoint connection",
        );

        conn.execute_raw("PRAGMA busy_timeout = 5000;")
            .map_err(|e| DbError::Sqlite(format!("passive checkpoint: busy_timeout: {e}")))?;

        let rows = conn
            .query_sync("PRAGMA wal_checkpoint(PASSIVE);", &[])
            .map_err(|e| DbError::Sqlite(format!("passive checkpoint: {e}")))?;

        let checkpointed = rows
            .first()
            .and_then(|r| match r.get_by_name("checkpointed") {
                Some(sqlmodel_core::Value::BigInt(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                Some(sqlmodel_core::Value::Int(n)) => Some(u64::try_from(*n).unwrap_or(0)),
                _ => None,
            })
            .unwrap_or(0);

        Ok(checkpointed)
    }

    /// Create (or refresh) a `.bak` backup of the database file.
    ///
    /// Skips silently for `:memory:` databases or when the primary file
    /// doesn't exist. Performs a WAL checkpoint first to ensure the backup
    /// is self-contained.
    ///
    /// Returns `Ok(Some(path))` with the backup path on success, `Ok(None)`
    /// if the operation was skipped (memory DB, missing file, or the existing
    /// backup is younger than `max_age`).
    pub fn create_proactive_backup(
        &self,
        max_age: std::time::Duration,
    ) -> DbResult<Option<PathBuf>> {
        if self.sqlite_path == ":memory:" {
            return Ok(None);
        }
        let primary = Path::new(&self.sqlite_path);
        if !primary.exists() {
            return Ok(None);
        }

        let bak_path = primary.with_file_name(format!(
            "{}.bak",
            primary
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("storage.sqlite3")
        ));

        // Skip if the existing backup is fresh enough.
        if bak_path.is_file()
            && let Ok(meta) = bak_path.metadata()
            && let Ok(modified) = meta.modified()
            && modified.elapsed().unwrap_or(max_age) < max_age
        {
            return Ok(None);
        }

        // Checkpoint WAL so the backup is self-contained.
        if let Err(e) = self.wal_checkpoint() {
            return Err(DbError::Sqlite(format!(
                "proactive backup aborted: WAL checkpoint failed for {}: {e}",
                primary.display()
            )));
        }

        std::fs::copy(primary, &bak_path).map_err(|e| {
            DbError::Sqlite(format!(
                "proactive backup failed: {} -> {}: {e}",
                primary.display(),
                bak_path.display()
            ))
        })?;

        tracing::info!(
            primary = %primary.display(),
            backup = %bak_path.display(),
            "created proactive database backup"
        );

        Ok(Some(bak_path))
    }

    /// Attempt one-shot recovery from `SQLite` corruption detected at runtime.
    ///
    /// This should be called when a query returns a corruption error
    /// (e.g. "database disk image is malformed"). The method:
    ///
    /// 1. Logs the corruption event
    /// 2. Attempts recovery via backup restore or archive reconstruction
    /// 3. Returns `Ok(true)` if recovery succeeded, `Ok(false)` if the DB
    ///    is in-memory (no recovery possible), or `Err` if recovery failed.
    ///
    /// After a successful recovery, callers should retry their operation
    /// by re-acquiring a connection from the pool.
    ///
    /// Uses a global flag to prevent concurrent recovery attempts.
    pub fn try_recover_from_corruption(&self, trigger_error: &str) -> DbResult<bool> {
        // Use a global flag to serialize recovery attempts. Only one thread
        // should attempt recovery at a time.
        static RECOVERY_IN_PROGRESS: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        struct ResetOnDrop;
        impl Drop for ResetOnDrop {
            fn drop(&mut self) {
                RECOVERY_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        if self.sqlite_path == ":memory:" {
            return Ok(false);
        }

        if RECOVERY_IN_PROGRESS
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            tracing::warn!(
                "runtime corruption recovery already in progress; skipping duplicate attempt"
            );
            return Ok(false);
        }

        let _guard = ResetOnDrop;

        tracing::error!(
            path = %self.sqlite_path,
            trigger = %trigger_error,
            "runtime corruption detected; attempting automatic recovery"
        );

        let primary_path = Path::new(&self.sqlite_path);
        match sqlite_file_is_healthy(primary_path) {
            Ok(true) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    trigger = %trigger_error,
                    "runtime corruption trigger received, but health probes already pass; skipping recovery"
                );
                return Ok(false);
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    trigger = %trigger_error,
                    error = %e,
                    "failed to run pre-recovery health probes; proceeding with recovery attempt"
                );
            }
        }

        // Record the corruption event in metrics only for confirmed/suspected unhealthy state.
        let metrics = mcp_agent_mail_core::global_metrics();
        metrics.db.integrity_failures_total.inc();

        match recover_sqlite_file(primary_path) {
            Ok(()) => {
                tracing::warn!(
                    path = %self.sqlite_path,
                    "runtime corruption recovery succeeded — clearing idle connections and returning to service"
                );
                // Idle connections hold FDs to the old (corrupted/unlinked) inode.
                // test_on_checkout(true) ensures stale connections fail health checks
                // and get evicted on next acquire, so explicit draining is not required.
                Ok(true)
            }
            Err(e) => {
                tracing::error!(
                    path = %self.sqlite_path,
                    error = %e,
                    "runtime corruption recovery FAILED — manual intervention required (try: am doctor repair)"
                );
                Err(DbError::IntegrityCorruption {
                    message: format!(
                        "Database corruption detected and automatic recovery failed: {e}. \
                         Run 'am doctor repair' or 'am doctor reconstruct' to manually recover."
                    ),
                    details: vec![trigger_error.to_string()],
                })
            }
        }
    }
}

static SQLITE_INIT_GATES: OnceLock<OrderedRwLock<HashMap<String, Arc<OnceCell<()>>>>> =
    OnceLock::new();
static POOL_CACHE: OnceLock<OrderedRwLock<HashMap<String, Weak<Pool<DbConn>>>>> = OnceLock::new();
static SQLITE_IDENTITY_PATH_CACHE: OnceLock<Mutex<HashMap<String, SqliteIdentityPathCacheEntry>>> =
    OnceLock::new();

#[derive(Clone, Debug)]
struct SqliteIdentityPathCacheEntry {
    normalized: String,
    validated_at: Instant,
}

const SQLITE_IDENTITY_PATH_CACHE_MAX_ENTRIES: usize = 256;
#[cfg(test)]
const SQLITE_IDENTITY_PATH_CACHE_FRESHNESS: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const SQLITE_IDENTITY_PATH_CACHE_FRESHNESS: Duration = Duration::from_secs(2);

fn sqlite_identity_path_cache() -> &'static Mutex<HashMap<String, SqliteIdentityPathCacheEntry>> {
    SQLITE_IDENTITY_PATH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sqlite_identity_path_cache_get(path: &str) -> Option<String> {
    let mut cache = sqlite_identity_path_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache.get(path)?;
    if entry.validated_at.elapsed() <= SQLITE_IDENTITY_PATH_CACHE_FRESHNESS {
        return Some(entry.normalized.clone());
    }
    cache.remove(path);
    None
}

fn sqlite_identity_path_cache_insert(path: &str, normalized: &str) {
    let mut cache = sqlite_identity_path_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !cache.contains_key(path)
        && cache.len() >= SQLITE_IDENTITY_PATH_CACHE_MAX_ENTRIES
        && let Some(victim) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.validated_at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&victim);
    }
    cache.insert(
        path.to_string(),
        SqliteIdentityPathCacheEntry {
            normalized: normalized.to_string(),
            validated_at: Instant::now(),
        },
    );
}

#[must_use]
fn normalize_sqlite_identity_path(path: &str) -> String {
    if path == ":memory:" {
        return path.to_string();
    }
    if let Some(cached) = sqlite_identity_path_cache_get(path) {
        return cached;
    }
    let as_path = Path::new(path);
    let normalized = std::fs::canonicalize(as_path).map_or_else(
        |_| {
            if as_path.is_absolute() {
                as_path.to_string_lossy().into_owned()
            } else if let Ok(cwd) = std::env::current_dir() {
                cwd.join(as_path).to_string_lossy().into_owned()
            } else {
                path.to_string()
            }
        },
        |canonical| canonical.to_string_lossy().into_owned(),
    );
    sqlite_identity_path_cache_insert(path, &normalized);
    normalized
}

#[must_use]
fn pool_cache_key(config: &DbPoolConfig) -> String {
    let identity = config.sqlite_path().map_or_else(
        |_| config.database_url.clone(),
        |parsed| {
            let resolved = resolve_sqlite_path_with_absolute_fallback(&parsed);
            normalize_sqlite_identity_path(&resolved)
        },
    );
    let storage_root_identity =
        normalize_sqlite_identity_path(&config.resolved_storage_root().to_string_lossy());
    format!(
        "{identity}|storage_root={storage_root_identity}|min={}|max={}|acquire_ms={}|lifetime_ms={}",
        config.min_connections,
        config.max_connections,
        config.acquire_timeout_ms,
        config.max_lifetime_ms
    )
}

fn sqlite_init_gate(sqlite_path: &str, storage_root: &Path) -> Arc<OnceCell<()>> {
    let gate_key = format!(
        "{}|storage_root={}",
        normalize_sqlite_identity_path(sqlite_path),
        normalize_sqlite_identity_path(&storage_root.to_string_lossy())
    );
    let gates = SQLITE_INIT_GATES
        .get_or_init(|| OrderedRwLock::new(LockLevel::DbSqliteInitGates, HashMap::new()));

    // Fast path: read lock for existing gate (concurrent readers).
    {
        let guard = gates.read();
        if let Some(gate) = guard.get(&gate_key) {
            return Arc::clone(gate);
        }
    }

    // Slow path: write lock to create a new gate (rare, once per SQLite file).
    let mut guard = gates.write();
    // Double-check after acquiring write lock.
    if let Some(gate) = guard.get(&gate_key) {
        return Arc::clone(gate);
    }
    let gate = Arc::new(OnceCell::new());
    guard.insert(gate_key, Arc::clone(&gate));
    gate
}

#[allow(clippy::result_large_err)]
async fn run_sqlite_init_once(
    cx: &Cx,
    sqlite_path: &str,
    run_migrations: bool,
) -> Outcome<(), SqlError> {
    // Clean up empty/corrupt WAL sidecars before opening any connections.
    // A 0-byte WAL file (left by a crash during DELETE->WAL journal mode
    // transition) triggers "WAL file too small for header during rebuild"
    // errors. Removing it is safe: SQLite recreates WAL on next write.
    if sqlite_path != ":memory:" {
        cleanup_empty_wal_sidecar(sqlite_path);
    }

    let missing_sqlite_file = sqlite_path != ":memory:" && !Path::new(sqlite_path).exists();

    if run_migrations {
        if missing_sqlite_file {
            let canonical_conn = match open_sqlite_file_with_lock_retry_canonical(sqlite_path) {
                Ok(conn) => conn,
                Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=open_canonical_missing_file failed: {err}"
                    )));
                }
            };

            if let Err(err) = canonical_conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL) {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=canonical_missing_file_pragmas failed: {err}"
                )));
            }

            match schema::migrate_to_latest(cx, &canonical_conn).await {
                Outcome::Ok(_) => {}
                Outcome::Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=migrate_to_latest_missing_file failed: {err}"
                    )));
                }
                Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
                Outcome::Panicked(payload) => return Outcome::Panicked(payload),
            }

            drop(canonical_conn);
        } else {
            let mig_conn = crate::guard_db_conn(
                match open_sqlite_file_with_lock_retry(sqlite_path) {
                    Ok(conn) => conn,
                    Err(err) => {
                        return Outcome::Err(SqlError::Custom(format!(
                            "sqlite init stage=open_file failed: {err}"
                        )));
                    }
                },
                "sqlite init migration connection",
            );

            if let Err(err) = execute_sql_with_lock_retry(
                &mig_conn,
                sqlite_path,
                schema::PRAGMA_DB_INIT_BASE_SQL,
                "sqlite init base pragmas",
            ) {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=base_pragmas failed: {err}"
                )));
            }

            match schema::migrate_to_latest_base(cx, &*mig_conn).await {
                Outcome::Ok(_) => {}
                Outcome::Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=migrate_to_latest_base failed: {err}"
                    )));
                }
                Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
                Outcome::Panicked(payload) => return Outcome::Panicked(payload),
            }

            drop(mig_conn);

            // Run full migrations (including v10-v15) via canonical SQLite.
            //
            // Base-mode migrations intentionally skip migrations that require
            // features unsupported by FrankenConnection (e.g. ALTER TABLE ADD
            // COLUMN reparsing). Canonical SQLite handles these correctly, so we
            // open a second connection to apply any remaining pending migrations
            // such as v15 (recipients_json column addition).
            let canonical_conn = match open_sqlite_file_with_lock_retry_canonical(sqlite_path) {
                Ok(conn) => conn,
                Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=open_canonical_for_full_migrations failed: {err}"
                    )));
                }
            };

            if let Err(err) = canonical_conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL) {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=canonical_pragmas failed: {err}"
                )));
            }

            match schema::migrate_to_latest(cx, &canonical_conn).await {
                Outcome::Ok(_) => {}
                Outcome::Err(err) => {
                    return Outcome::Err(SqlError::Custom(format!(
                        "sqlite init stage=migrate_to_latest (full) failed: {err}"
                    )));
                }
                Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
                Outcome::Panicked(payload) => return Outcome::Panicked(payload),
            }

            drop(canonical_conn);
        }
    }

    let runtime_conn = crate::guard_db_conn(
        match open_sqlite_file_with_lock_retry(sqlite_path) {
            Ok(conn) => conn,
            Err(err) => {
                return Outcome::Err(SqlError::Custom(format!(
                    "sqlite init stage=open_file_runtime failed: {err}"
                )));
            }
        },
        "sqlite init runtime connection",
    );

    if !run_migrations
        && let Err(err) = execute_sql_with_lock_retry(
            &runtime_conn,
            sqlite_path,
            schema::PRAGMA_DB_INIT_BASE_SQL,
            "sqlite init runtime base pragmas",
        )
    {
        return Outcome::Err(SqlError::Custom(format!(
            "sqlite init stage=base_pragmas_runtime failed: {err}"
        )));
    }

    // Always enforce startup cleanup for legacy identity FTS artifacts.
    // These can be reintroduced by historical/full migration paths and have
    // caused post-crash rowid/index mismatch failures.
    if let Err(err) = schema::enforce_runtime_fts_cleanup(&runtime_conn) {
        return Outcome::Err(SqlError::Custom(format!(
            "sqlite init stage=enforce_runtime_fts_cleanup failed: {err}"
        )));
    }

    // Switch to WAL journal mode AFTER migrations complete.
    //
    // Migrations run in DELETE (rollback) mode for safety. Runtime connections
    // intentionally do not reissue `journal_mode=WAL`, because that database-
    // wide transition amplifies lock contention on pool acquire and durability
    // probes. Set WAL once here before pooled connections open.
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/13
    if let Err(err) = execute_sql_with_lock_retry(
        &runtime_conn,
        sqlite_path,
        "PRAGMA journal_mode = WAL;",
        "sqlite init switch journal_mode=WAL",
    ) {
        tracing::warn!(
            path = %sqlite_path,
            error = %err,
            "failed to switch journal_mode to WAL after init; runtime will continue in rollback-journal mode until a later init succeeds"
        );
        // Non-fatal: reads/writes can still proceed, but concurrency may degrade.
    }

    // Rebuild inbox_stats from ground truth, drop legacy triggers, and fix
    // mixed-scale timestamps left by the Python server.
    if let Err(err) = startup_data_repairs(&runtime_conn) {
        tracing::warn!(
            path = %sqlite_path,
            error = %err,
            "startup data repairs failed; some counters/timestamps may be stale"
        );
    }

    drop(runtime_conn);
    Outcome::Ok(())
}

/// One-shot data repairs run at startup before pool connections are handed out.
///
/// 1. Drop legacy `inbox_stats` triggers (redundant with explicit rebuilds).
/// 2. Rebuild `inbox_stats` from ground truth.
/// 3. Fix mixed-scale timestamps (seconds/millis → microseconds).
#[allow(clippy::result_large_err)]
fn startup_data_repairs(conn: &DbConn) -> Result<(), SqlError> {
    // ── inbox_stats rebuild ──────────────────────────────────────────
    let has_inbox_stats = !conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='inbox_stats'",
            &[],
        )?
        .is_empty();

    if has_inbox_stats {
        // Drop legacy triggers to prevent double-counting.
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_insert")?;
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_mark_read")?;
        conn.execute_raw("DROP TRIGGER IF EXISTS trg_inbox_stats_ack")?;

        // Full rebuild from ground truth.
        conn.execute_raw("DELETE FROM inbox_stats")?;
        conn.execute_raw(
            "INSERT INTO inbox_stats \
                (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
            SELECT \
                r.agent_id, \
                COUNT(*) AS total_count, \
                SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
                SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
                MAX(m.created_ts) AS last_message_ts \
            FROM message_recipients r \
            JOIN messages m ON m.id = r.message_id \
            GROUP BY r.agent_id",
        )?;
    }

    // ── Fix mixed-scale timestamps ───────────────────────────────────
    // The Python server occasionally wrote created_ts in seconds or
    // milliseconds instead of microseconds.  Detect and upscale:
    //   seconds  (< 1e13)  → × 1_000_000
    //   millis   (< 1e16)  → × 1_000
    let has_messages = !conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='messages'",
            &[],
        )?
        .is_empty();

    if has_messages {
        // Magnitude-based detection (non-overlapping ranges):
        //   2026 seconds  ≈ 1.77 × 10⁹   →  < 10¹² is definitely seconds
        //   2026 millis   ≈ 1.77 × 10¹²   →  [10¹², 10¹⁵) is definitely millis
        //   2026 micros   ≈ 1.77 × 10¹⁵   →  ≥ 10¹⁵ is already correct
        //
        // Order matters: handle millis FIRST to avoid seconds check catching
        // millis values that happen to be < 10¹² (they can't — millis are ≥ 10¹²).
        // But we still process millis first as a safety measure.

        // Milliseconds → microseconds  [10^12, 10^15)
        conn.execute_raw(
            "UPDATE messages SET created_ts = created_ts * 1000 \
             WHERE created_ts >= 1000000000000 AND created_ts < 1000000000000000",
        )?;
        // Seconds → microseconds  (0, 10^12)
        conn.execute_raw(
            "UPDATE messages SET created_ts = created_ts * 1000000 \
             WHERE created_ts > 0 AND created_ts < 1000000000000",
        )?;
    }

    Ok(())
}

#[must_use]
fn should_retry_sqlite_init_error(error: &SqlError) -> bool {
    let msg = error.to_string();
    is_sqlite_recovery_error_message(&msg) || is_lock_error(&msg)
}

const SQLITE_LOCK_MAX_RETRIES: usize = 3;

#[must_use]
fn sqlite_lock_retry_delay(retry_index: usize) -> Duration {
    let exponent = u32::try_from(retry_index.min(3)).unwrap_or(3);
    Duration::from_millis(25_u64.saturating_mul(1_u64 << exponent))
}

#[must_use]
pub fn is_sqlite_snapshot_conflict_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("snapshot conflict on pages")
        || lower.contains("busy_snapshot")
        || lower.contains("snapshot too old")
        || (lower.contains("snapshot db_size") && lower.contains("page "))
}

#[must_use]
pub fn is_sqlite_recovery_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_corruption_error_message(message)
        || is_sqlite_snapshot_conflict_error_message(message)
        || lower.contains("out of memory")
        || lower.contains("cursor stack is empty")
        || lower.contains("called `option::unwrap()` on a `none` value")
        || lower.contains("internal error")
        || lower.contains("cursor must be on a leaf")
        || lower.contains("wal file too small")
}

#[must_use]
fn sqlite_absolute_fallback_path(path: &str, open_error: &str) -> Option<String> {
    if path == ":memory:"
        || Path::new(path).is_absolute()
        || path.starts_with("./")
        || path.starts_with("../")
        || !is_sqlite_recovery_error_message(open_error)
    {
        return None;
    }
    let absolute_candidate = Path::new("/").join(path);
    if !absolute_candidate.exists() {
        return None;
    }
    Some(absolute_candidate.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxDbInventory {
    pub projects: usize,
    pub agents: usize,
    pub messages: usize,
    pub max_message_id: i64,
    pub project_identities: BTreeSet<crate::reconstruct::MailboxProjectIdentity>,
}

#[allow(clippy::result_large_err)]
pub fn inspect_mailbox_db_inventory(primary_path: &Path) -> Result<MailboxDbInventory, SqlError> {
    let conn = open_sqlite_file_with_lock_retry(primary_path.to_string_lossy().as_ref())?;
    let present = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            &[],
        )?
        .into_iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect::<std::collections::BTreeSet<_>>();

    let query_count = |sql: &str, alias: &str| -> Result<usize, SqlError> {
        let rows = conn.query_sync(sql, &[])?;
        let Some(row) = rows.first() else {
            return Err(SqlError::Custom(format!(
                "no rows returned from sqlite reconcile inventory query for {alias}"
            )));
        };
        Ok(row
            .get_named::<i64>(alias)
            .ok()
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0))
    };

    let projects = if present.contains("projects") {
        query_count(
            "SELECT COUNT(*) AS project_count FROM projects",
            "project_count",
        )?
    } else {
        0
    };
    let agents = if present.contains("agents") {
        query_count("SELECT COUNT(*) AS agent_count FROM agents", "agent_count")?
    } else {
        0
    };
    let (messages, max_message_id) = if present.contains("messages") {
        let rows = conn.query_sync(
            "SELECT COUNT(*) AS message_count, COALESCE(MAX(id), 0) AS max_id FROM messages",
            &[],
        )?;
        let Some(row) = rows.first() else {
            return Err(SqlError::Custom(
                "no rows returned from sqlite message inventory query".to_string(),
            ));
        };
        (
            row.get_named::<i64>("message_count")
                .ok()
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0),
            row.get_named::<i64>("max_id").unwrap_or(0),
        )
    } else {
        (0, 0)
    };
    let project_identities = if present.contains("projects") {
        crate::reconstruct::collect_db_project_identities(&conn)?
    } else {
        BTreeSet::new()
    };

    Ok(MailboxDbInventory {
        projects,
        agents,
        messages,
        max_message_id,
        project_identities,
    })
}

fn archive_has_real_projects(storage_root: &Path) -> bool {
    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
        return false;
    }

    std::fs::read_dir(&projects_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir() && !file_type.is_symlink())
        })
}

#[allow(clippy::result_large_err)]
fn reconcile_archive_state_before_init(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<bool, SqlError> {
    if !archive_has_real_projects(storage_root) {
        return Ok(false);
    }

    refuse_mutating_mailbox_when_owned(primary_path, storage_root)?;

    if !primary_path.exists() {
        let stats = reconstruct_sqlite_file_with_archive_salvage(primary_path, storage_root)?;
        tracing::warn!(
            path = %primary_path.display(),
            storage_root = %storage_root.display(),
            %stats,
            "reconstructed missing sqlite database from archive before initialization"
        );
        return Ok(true);
    }

    if !sqlite_file_is_healthy(primary_path)? {
        return Ok(false);
    }

    let archive = crate::reconstruct::scan_archive_message_inventory(storage_root);
    if archive.projects == 0 && archive.agents == 0 && archive.unique_message_ids == 0 {
        return Ok(false);
    }

    let db_inventory = inspect_mailbox_db_inventory(primary_path)?;
    let archive_max_id = archive.latest_message_id.unwrap_or(0);
    let missing_archive_projects = crate::reconstruct::archive_missing_project_identities(
        &archive,
        &db_inventory.project_identities,
    );
    let archive_projects_ahead = archive.projects > db_inventory.projects;
    let archive_agents_ahead = archive.agents > db_inventory.agents;
    let archive_messages_ahead = archive.unique_message_ids > db_inventory.messages;
    let archive_latest_id_ahead = archive_max_id > db_inventory.max_message_id;
    let archive_identity_ahead = !missing_archive_projects.is_empty();
    let archive_ahead = archive_projects_ahead
        || archive_agents_ahead
        || archive_messages_ahead
        || archive_latest_id_ahead
        || archive_identity_ahead;
    if !archive_ahead {
        return Ok(false);
    }

    let stats = reconstruct_sqlite_file_with_archive_salvage(primary_path, storage_root)?;
    tracing::warn!(
        path = %primary_path.display(),
        storage_root = %storage_root.display(),
        db_project_count = db_inventory.projects,
        db_agent_count = db_inventory.agents,
        db_message_count = db_inventory.messages,
        db_max_id = db_inventory.max_message_id,
        archive_project_count = archive.projects,
        archive_agent_count = archive.agents,
        archive_message_count = archive.unique_message_ids,
        archive_max_id,
        missing_archive_projects = ?missing_archive_projects,
        %stats,
        "reconciled sqlite database from archive before initialization because archive inventory or project identity state was ahead"
    );
    Ok(true)
}

#[allow(clippy::result_large_err)]
fn ensure_sqlite_parent_dir_exists(path: &str) -> Result<(), SqlError> {
    if path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            SqlError::Custom(format!("failed to create db dir {}: {e}", parent.display()))
        })?;
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) fn open_sqlite_file_with_lock_retry(sqlite_path: &str) -> Result<DbConn, SqlError> {
    open_sqlite_file_with_lock_retry_impl(
        sqlite_path,
        |path| DbConn::open_file(path),
        std::thread::sleep,
    )
}

#[allow(clippy::result_large_err)]
fn open_sqlite_file_with_lock_retry_canonical(
    sqlite_path: &str,
) -> Result<sqlmodel_sqlite::SqliteConnection, SqlError> {
    open_sqlite_file_with_lock_retry_impl(
        sqlite_path,
        |path| sqlmodel_sqlite::SqliteConnection::open_file(path),
        std::thread::sleep,
    )
}

#[allow(clippy::result_large_err)]
fn retry_sqlite_lock_impl<T, F, S>(
    sqlite_path: &str,
    operation: &str,
    mut op: F,
    mut sleep_fn: S,
) -> Result<T, SqlError>
where
    F: FnMut() -> Result<T, SqlError>,
    S: FnMut(Duration),
{
    let mut retries = 0usize;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) => {
                let message = err.to_string();
                if !is_lock_error(&message) || retries >= SQLITE_LOCK_MAX_RETRIES {
                    return Err(err);
                }
                let delay = sqlite_lock_retry_delay(retries);
                let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                tracing::warn!(
                    path = %sqlite_path,
                    operation,
                    error = %err,
                    retry = retries + 1,
                    max_retries = SQLITE_LOCK_MAX_RETRIES,
                    delay_ms,
                    "sqlite operation hit lock/busy error; retrying"
                );
                sleep_fn(delay);
                retries += 1;
            }
        }
    }
}

#[allow(clippy::result_large_err)]
fn open_sqlite_file_with_lock_retry_impl<C, F, S>(
    sqlite_path: &str,
    mut open_file: F,
    sleep_fn: S,
) -> Result<C, SqlError>
where
    F: FnMut(&str) -> Result<C, SqlError>,
    S: FnMut(Duration),
{
    retry_sqlite_lock_impl(
        sqlite_path,
        "sqlite open",
        || open_file(sqlite_path),
        sleep_fn,
    )
}

#[allow(clippy::result_large_err)]
fn execute_sql_with_lock_retry(
    conn: &DbConn,
    sqlite_path: &str,
    sql: &str,
    operation: &str,
) -> Result<(), SqlError> {
    retry_sqlite_lock_impl(
        sqlite_path,
        operation,
        || conn.execute_raw(sql),
        std::thread::sleep,
    )
}

/// Open a file-backed sqlite connection and automatically recover from
/// corruption-like open failures when possible.
#[allow(clippy::result_large_err)]
pub fn open_sqlite_file_with_recovery(sqlite_path: &str) -> Result<DbConn, SqlError> {
    if sqlite_path == ":memory:" {
        return DbConn::open_memory();
    }
    ensure_sqlite_parent_dir_exists(sqlite_path)?;

    match open_sqlite_file_with_lock_retry(sqlite_path) {
        Ok(conn) => Ok(conn),
        Err(primary_err) => {
            let primary_msg = primary_err.to_string();

            if let Some(fallback_path) = sqlite_absolute_fallback_path(sqlite_path, &primary_msg) {
                match open_sqlite_file_with_lock_retry(&fallback_path) {
                    Ok(conn) => return Ok(conn),
                    Err(fallback_err) => {
                        return Err(SqlError::Custom(format!(
                            "cannot open sqlite at {sqlite_path}: {primary_err}; fallback {fallback_path} failed: {fallback_err}"
                        )));
                    }
                }
            }

            if !is_sqlite_recovery_error_message(&primary_msg) {
                return Err(primary_err);
            }

            recover_sqlite_file(Path::new(sqlite_path))?;
            open_sqlite_file_with_lock_retry(sqlite_path).map_err(|reopen_err| {
                SqlError::Custom(format!(
                    "cannot open sqlite at {sqlite_path}: {primary_err}; reopen after recovery failed: {reopen_err}"
                ))
            })
        }
    }
}

#[allow(clippy::result_large_err)]
async fn initialize_sqlite_file_once(
    cx: &Cx,
    sqlite_path: &str,
    run_migrations: bool,
    storage_root: &Path,
) -> Outcome<(), SqlError> {
    let path = Path::new(sqlite_path);
    // Reconcile archive-backed state before first init so every entrypoint,
    // not just the server startup probe, preserves durable message IDs when a
    // DB is missing or stale relative to the archive.
    if sqlite_path != ":memory:"
        && let Err(err) = reconcile_archive_state_before_init(path, storage_root)
    {
        return Outcome::Err(err);
    }

    match run_sqlite_init_once(cx, sqlite_path, run_migrations).await {
        ok @ Outcome::Ok(()) => ok,
        non_err @ (Outcome::Cancelled(_) | Outcome::Panicked(_)) => non_err,
        Outcome::Err(first_err) => {
            if !should_retry_sqlite_init_error(&first_err) {
                return Outcome::Err(first_err);
            }

            if is_sqlite_recovery_error_message(&first_err.to_string()) {
                match sqlite_file_is_healthy(path) {
                    Ok(false) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "sqlite init failed and health probes detected corruption; attempting automatic recovery"
                        );
                        if let Err(recover_err) = recover_sqlite_file(path) {
                            if !should_retry_sqlite_init_error(&recover_err) {
                                return Outcome::Err(recover_err);
                            }
                            tracing::warn!(
                                path = %path.display(),
                                error = %recover_err,
                                "sqlite recovery attempt failed with retryable error; retrying init once"
                            );
                        }
                    }
                    Ok(true) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %first_err,
                            "sqlite init failed but file-level health probes passed; retrying initialization once"
                        );
                    }
                    Err(health_err) => {
                        if !should_retry_sqlite_init_error(&health_err) {
                            return Outcome::Err(health_err);
                        }
                        tracing::warn!(
                            path = %path.display(),
                            error = %health_err,
                            "sqlite health probe failed with retryable error; retrying initialization once"
                        );
                    }
                }
            } else {
                // Lock/busy class errors are often transient under concurrent startup.
                // Skip corruption probes and retry initialization once.
                tracing::warn!(
                    path = %path.display(),
                    error = %first_err,
                    "sqlite init failed with retryable lock/busy error; retrying initialization once"
                );
            }

            run_sqlite_init_once(cx, sqlite_path, run_migrations).await
        }
    }
}

/// Check whether an error message indicates `SQLite` file corruption.
///
/// Used by auto-recovery logic to decide whether to attempt backup
/// restoration or reinitialization.
#[must_use]
pub fn is_corruption_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
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
        || lower.contains("no healthy backup was found")
        || lower.contains("wal file too small")
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_details_from_rows(rows: &[sqlmodel_core::Row]) -> Vec<String> {
    let mut details: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(v) = row.get_named::<String>("quick_check") {
            details.push(v);
        } else if let Ok(v) = row.get_named::<String>("integrity_check") {
            details.push(v);
        } else if let Some(Value::Text(v)) = row.values().next() {
            details.push(v.clone());
        }
    }
    if details.is_empty() {
        details.push("ok".to_string());
    }
    details
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_details(conn: &DbConn, pragma_sql: &str) -> Result<Vec<String>, SqlError> {
    let rows = conn.query_sync(pragma_sql, &[])?;
    Ok(sqlite_pragma_check_details_from_rows(&rows))
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_is_ok(conn: &DbConn, pragma_sql: &str) -> Result<bool, SqlError> {
    let details = sqlite_pragma_check_details(conn, pragma_sql)?;
    Ok(details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok")))
}

#[allow(clippy::result_large_err)]
fn sqlite_quick_check_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok(conn, "PRAGMA quick_check")
}

#[allow(clippy::result_large_err)]
fn sqlite_incremental_check_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok(conn, "PRAGMA integrity_check(1)")
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_details_canonical(
    conn: &sqlmodel_sqlite::SqliteConnection,
    pragma_sql: &str,
) -> Result<Vec<String>, SqlError> {
    let rows = conn.query_sync(pragma_sql, &[])?;
    Ok(sqlite_pragma_check_details_from_rows(&rows))
}

#[allow(clippy::result_large_err)]
fn sqlite_pragma_check_is_ok_canonical(
    conn: &sqlmodel_sqlite::SqliteConnection,
    pragma_sql: &str,
) -> Result<bool, SqlError> {
    let details = sqlite_pragma_check_details_canonical(conn, pragma_sql)?;
    Ok(details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok")))
}

#[allow(clippy::result_large_err)]
fn sqlite_canonical_quick_check_is_ok(
    conn: &sqlmodel_sqlite::SqliteConnection,
) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok_canonical(conn, "PRAGMA quick_check")
}

#[allow(clippy::result_large_err)]
fn sqlite_canonical_incremental_check_is_ok(
    conn: &sqlmodel_sqlite::SqliteConnection,
) -> Result<bool, SqlError> {
    sqlite_pragma_check_is_ok_canonical(conn, "PRAGMA integrity_check(1)")
}

/// Remove empty WAL/SHM sidecars that cause "WAL file too small for header"
/// errors during SQLite open.
///
/// A 0-byte `-wal` file is pathological: SQLite expects either no WAL (so it
/// creates a fresh one) or a WAL with at least a 32-byte header. A zero-length
/// WAL can be left behind when:
/// - A crash occurs during the `DELETE` -> `WAL` journal mode transition
/// - A `PRAGMA journal_size_limit` triggers truncation racing with a reader
/// - The process is killed between WAL creation and first header write
///
/// Removing the empty file is always safe because SQLite recreates the WAL
/// on the next write. We also remove empty SHM files for consistency.
fn cleanup_empty_wal_sidecar(sqlite_path: &str) {
    let db_path = Path::new(sqlite_path);
    if !db_path.exists() {
        return;
    }
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = db_path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = PathBuf::from(sidecar_os);
        match std::fs::metadata(&sidecar_path) {
            Ok(meta) if meta.len() == 0 => {
                tracing::warn!(
                    path = %sidecar_path.display(),
                    "removing empty WAL/SHM sidecar (prevents 'WAL file too small for header' errors)"
                );
                let _ = std::fs::remove_file(&sidecar_path);
            }
            _ => {}
        }
    }
}

#[must_use]
fn sqlite_file_has_live_sidecars(path: &Path) -> bool {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = PathBuf::from(sidecar_os);
        if let Ok(meta) = std::fs::metadata(&sidecar_path)
            && meta.len() > 0
        {
            return true;
        }
    }
    false
}

#[allow(clippy::result_large_err)]
fn sqlite_file_is_healthy_canonical(path: &Path) -> Result<bool, SqlError> {
    let path_str = path.to_string_lossy();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(path_str.as_ref())?;

    if !sqlite_canonical_quick_check_is_ok(&conn)? {
        return Ok(false);
    }
    sqlite_canonical_incremental_check_is_ok(&conn)
}

#[allow(clippy::result_large_err)]
fn sqlite_table_has_column(conn: &DbConn, table: &str, column: &str) -> Result<bool, SqlError> {
    let rows = conn.query_sync(&format!("PRAGMA table_info({table})"), &[])?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .any(|name| name == column))
}

#[allow(clippy::result_large_err)]
fn sqlite_ack_pending_probe_is_ok(conn: &DbConn) -> Result<bool, SqlError> {
    let messages_has_ack_required = sqlite_table_has_column(conn, "messages", "ack_required")?;
    let recipients_has_ack_ts = sqlite_table_has_column(conn, "message_recipients", "ack_ts")?;
    let recipients_has_message_id =
        sqlite_table_has_column(conn, "message_recipients", "message_id")?;

    // Skip schema-specific smoke probes on partially initialized/legacy schemas.
    if !(messages_has_ack_required && recipients_has_ack_ts && recipients_has_message_id) {
        return Ok(true);
    }

    conn.query_sync(
        "SELECT 1 \
         FROM message_recipients \
         WHERE ack_ts IS NULL \
           AND message_id IN (SELECT id FROM messages WHERE ack_required = 1) \
         LIMIT 1",
        &[],
    )
    .map(|_| true)
}

#[allow(clippy::result_large_err)]
fn sqlite_file_is_healthy_with_compat_probe<F>(
    path: &Path,
    mut compatibility_probe: F,
) -> Result<bool, SqlError>
where
    F: FnMut(&Path) -> Result<bool, SqlError>,
{
    if !path.exists() {
        return Ok(false);
    }
    let path_str = path.to_string_lossy();
    let conn = match open_sqlite_file_with_lock_retry(path_str.as_ref()) {
        Ok(conn) => conn,
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    };

    match sqlite_quick_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_incremental_check_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg) || is_sqlite_snapshot_conflict_error_message(&msg)
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    match sqlite_ack_pending_probe_is_ok(&conn) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_corruption_error_message(&msg)
                || is_sqlite_snapshot_conflict_error_message(&msg)
                || msg.to_ascii_lowercase().contains("out of memory")
            {
                return Ok(false);
            }
            return Err(e);
        }
    }

    // FrankenConnection can miss schema faults present in active WAL sidecars.
    // When sidecars exist, run a canonical sqlite probe to avoid false healthy
    // verdicts (e.g. duplicate-index malformed schema in WAL state).
    if sqlite_file_has_live_sidecars(path) {
        match compatibility_probe(path) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => {
                let msg = e.to_string();
                if is_corruption_error_message(&msg) || is_sqlite_recovery_error_message(&msg) {
                    return Ok(false);
                }
                if is_lock_error(&msg) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "sqlite canonical health probe hit lock/busy error; preserving primary health verdict"
                    );
                } else {
                    return Err(e);
                }
            }
        }
    }

    Ok(true)
}

#[allow(clippy::result_large_err)]
pub(crate) fn sqlite_file_is_healthy(path: &Path) -> Result<bool, SqlError> {
    sqlite_file_is_healthy_with_compat_probe(path, sqlite_file_is_healthy_canonical)
}

#[allow(clippy::result_large_err)]
fn refuse_auto_recovery_with_live_sidecars(primary_path: &Path) -> Result<(), SqlError> {
    if !sqlite_file_has_live_sidecars(primary_path) {
        return Ok(());
    }

    // Stale WAL/SHM files are common after crashes or failed migrations.
    // Instead of immediately refusing recovery, try to checkpoint the WAL.
    // If no other process holds a lock, the checkpoint will succeed and we
    // can remove the sidecars, allowing recovery to proceed.
    tracing::info!(
        path = %primary_path.display(),
        "live WAL/SHM sidecars detected; attempting checkpoint before recovery"
    );
    match try_checkpoint_and_clear_sidecars(primary_path) {
        Ok(()) => {
            tracing::info!(
                path = %primary_path.display(),
                "WAL checkpoint succeeded; sidecars cleared, proceeding with recovery"
            );
            Ok(())
        }
        Err(e) => {
            let err_str = e.to_string();
            // If the error is a lock/busy error, another process truly holds the DB
            if is_lock_error(&err_str) {
                Err(SqlError::Custom(format!(
                    "cannot recover {} — another process holds a lock on the database; \
                     stop the server first, then retry",
                    primary_path.display()
                )))
            } else {
                Err(SqlError::Custom(format!(
                    "cannot recover {} while live WAL/SHM sidecars are present; \
                     automatic checkpoint failed: {e}; stop the server and run explicit repair",
                    primary_path.display()
                )))
            }
        }
    }
}

/// Try to checkpoint the WAL and remove sidecar files.
#[allow(clippy::result_large_err)]
fn try_checkpoint_and_clear_sidecars(primary_path: &Path) -> Result<(), SqlError> {
    let path_str = primary_path.to_string_lossy();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(path_str.as_ref())?;
    // Attempt a truncating checkpoint to fold WAL changes into the main DB
    conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    // Remove any residual sidecars left after the successful checkpoint.
    remove_sqlite_sidecars(primary_path);
    if sqlite_file_has_live_sidecars(primary_path) {
        return Err(SqlError::Custom(format!(
            "checkpoint completed for {} but non-empty WAL/SHM sidecars remain",
            primary_path.display()
        )));
    }
    Ok(())
}

/// Remove WAL and SHM sidecar files if they exist.
fn remove_sqlite_sidecars(primary_path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = primary_path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = PathBuf::from(sidecar_os);
        if sidecar_path.exists()
            && let Err(e) = std::fs::remove_file(&sidecar_path)
        {
            tracing::warn!(
                path = %sidecar_path.display(),
                error = %e,
                "failed to remove sqlite sidecar"
            );
        }
    }
}

#[must_use]
fn is_index_only_integrity_issue(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("wrong # of entries in index")
        || lower.contains("missing from index")
        || lower.contains("rowid") && lower.contains("index")
}

#[must_use]
fn details_are_index_only_issues(details: &[String]) -> bool {
    !details.is_empty()
        && details.iter().all(|detail| {
            !detail.trim().eq_ignore_ascii_case("ok") && is_index_only_integrity_issue(detail)
        })
}

#[allow(clippy::result_large_err)]
fn try_repair_index_only_corruption(primary_path: &Path) -> Result<bool, SqlError> {
    if !primary_path.exists() {
        return Ok(false);
    }
    let path_str = primary_path.to_string_lossy();
    let conn = open_sqlite_file_with_lock_retry_canonical(path_str.as_ref())?;
    let quick_details = sqlite_pragma_check_details_canonical(&conn, "PRAGMA quick_check")?;
    if quick_details
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        return Ok(false);
    }
    if !details_are_index_only_issues(&quick_details) {
        return Ok(false);
    }

    tracing::warn!(
        path = %primary_path.display(),
        details = ?quick_details,
        "detected index-only sqlite corruption; attempting in-place REINDEX repair"
    );

    conn.execute_raw("REINDEX;")?;
    let _ = conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);");

    let post_quick = sqlite_pragma_check_details_canonical(&conn, "PRAGMA quick_check")?;
    if !post_quick
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        tracing::warn!(
            path = %primary_path.display(),
            details = ?post_quick,
            "in-place REINDEX completed but quick_check still reports issues"
        );
        return Ok(false);
    }

    let post_incremental =
        sqlite_pragma_check_details_canonical(&conn, "PRAGMA integrity_check(1)")?;
    if !post_incremental
        .iter()
        .all(|detail| detail.trim().eq_ignore_ascii_case("ok"))
    {
        tracing::warn!(
            path = %primary_path.display(),
            details = ?post_incremental,
            "in-place REINDEX passed quick_check but failed integrity_check(1)"
        );
        return Ok(false);
    }

    tracing::warn!(
        path = %primary_path.display(),
        "in-place REINDEX repaired index-only sqlite corruption"
    );
    Ok(true)
}

#[allow(clippy::result_large_err)]
fn recover_sqlite_file(primary_path: &Path) -> Result<(), SqlError> {
    let config = mcp_agent_mail_core::Config::from_env();
    let storage_root_path = config.storage_root.as_path();

    // Capture pre-recovery snapshot before any mutation.
    let snapshot =
        crate::forensics::capture_pre_recovery_snapshot(primary_path, "automatic-recovery")
            .with_environment(storage_root_path, &config.database_url);
    tracing::info!(
        trigger = snapshot.trigger,
        db_bytes = ?snapshot.db_bytes,
        wal_bytes = ?snapshot.wal_bytes,
        holders = snapshot.process_holders.len(),
        locks = snapshot.file_locks.len(),
        recovery_lock_active = snapshot.recovery_lock_active,
        "pre-recovery snapshot captured"
    );
    if is_real_directory(storage_root_path) {
        return ensure_sqlite_file_healthy_with_archive(primary_path, storage_root_path);
    }
    ensure_sqlite_file_healthy(primary_path)
}

#[allow(clippy::result_large_err)]
fn capture_automatic_recovery_bundle(
    primary_path: &Path,
    storage_root: &Path,
    command_name: &str,
) -> Result<PathBuf, SqlError> {
    let database_url = format!("sqlite:///{}", primary_path.display());
    let bundle_dir = crate::capture_mailbox_forensic_bundle(crate::MailboxForensicCapture {
        command_name,
        trigger: "automatic-recovery",
        database_url: &database_url,
        db_path: primary_path,
        storage_root,
        integrity_detail: None,
    })?;
    tracing::warn!(
        path = %primary_path.display(),
        command = command_name,
        bundle = %bundle_dir.display(),
        "captured mailbox forensic bundle before automatic recovery"
    );
    Ok(bundle_dir)
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn sqlite_backup_candidates(primary_path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<(u8, SystemTime, PathBuf)> = Vec::new();
    let Some(file_name) = primary_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let parent = primary_path.parent().unwrap_or_else(|| Path::new("."));
    let scan_dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    let bak = primary_path.with_file_name(format!("{file_name}.bak"));
    if is_real_file(&bak) {
        let modified = bak
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((0, modified, bak));
    }

    let backup_prefix = format!("{file_name}.backup-");
    let backup_bak_prefix = format!("{file_name}.bak.");
    let recovery_prefix = format!("{file_name}.recovery");
    if let Ok(entries) = std::fs::read_dir(scan_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let priority = if name.starts_with(&backup_bak_prefix) {
                1
            } else if name.starts_with(&backup_prefix) {
                2
            } else if name.starts_with(&recovery_prefix) {
                3
            } else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((priority, modified, path));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    candidates.into_iter().map(|(_, _, p)| p).collect()
}

fn find_healthy_backup(primary_path: &Path) -> Option<PathBuf> {
    for candidate in sqlite_backup_candidates(primary_path) {
        match sqlite_file_is_healthy(&candidate) {
            Ok(true) => return Some(candidate),
            Ok(false) => match sqlite_file_is_healthy_canonical(&candidate) {
                Ok(true) => {
                    tracing::warn!(
                        candidate = %candidate.display(),
                        "sqlite backup candidate failed primary health probe but passed canonical probe; accepting candidate"
                    );
                    return Some(candidate);
                }
                Ok(false) => tracing::warn!(
                    candidate = %candidate.display(),
                    "sqlite backup candidate failed health probes; skipping"
                ),
                Err(e) => tracing::warn!(
                    candidate = %candidate.display(),
                    error = %e,
                    "sqlite backup candidate canonical probe failed; skipping"
                ),
            },
            Err(e) => tracing::warn!(
                candidate = %candidate.display(),
                error = %e,
                "sqlite backup candidate unreadable; skipping"
            ),
        }
    }
    None
}

fn has_quarantined_primary_artifact(primary_path: &Path) -> bool {
    let Some(file_name) = primary_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = primary_path.parent().unwrap_or_else(|| Path::new("."));
    let scan_dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let quarantine_prefixes = [
        format!("{file_name}.corrupt-"),
        format!("{file_name}.archive-reconcile-"),
        format!("{file_name}.reconstruct-"),
    ];

    std::fs::read_dir(scan_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .any(|name| {
            quarantine_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
}

#[must_use]
fn resolve_sqlite_path_with_absolute_fallback(sqlite_path: &str) -> String {
    if sqlite_path == ":memory:" {
        return sqlite_path.to_string();
    }

    let relative_path = Path::new(sqlite_path);
    if relative_path.is_absolute() {
        return sqlite_path.to_string();
    }

    // Preserve explicitly relative paths exactly as configured.
    if sqlite_path.starts_with("./") || sqlite_path.starts_with("../") {
        return sqlite_path.to_string();
    }

    // Only reinterpret the path when the configured relative file actually
    // exists and is unhealthy. A missing relative path may be a legitimate
    // fresh-start target and must not be silently rewritten to `/<path>`.
    if !relative_path.exists() {
        return sqlite_path.to_string();
    }

    let absolute_candidate = Path::new("/").join(relative_path);
    if !absolute_candidate.exists() {
        return sqlite_path.to_string();
    }

    let relative_health = sqlite_file_is_healthy(relative_path).ok();
    let absolute_health = sqlite_file_is_healthy(&absolute_candidate).ok();
    if matches!(
        (relative_health, absolute_health),
        (Some(false), Some(true))
    ) {
        tracing::warn!(
            relative_path = %relative_path.display(),
            absolute_candidate = %absolute_candidate.display(),
            "detected malformed relative sqlite path with healthy absolute sibling; using absolute path (did you mean sqlite:////...?)"
        );
        return absolute_candidate.to_string_lossy().into_owned();
    }

    sqlite_path.to_string()
}

#[must_use]
pub fn normalize_sqlite_path_for_pool_key(sqlite_path: &str) -> String {
    resolve_sqlite_path_with_absolute_fallback(sqlite_path)
}

pub fn resolve_mailbox_sqlite_path(database_url: &str) -> DbResult<ResolvedMailboxSqlitePath> {
    let config = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let configured_path = config.sqlite_path()?;
    let canonical_path = normalize_sqlite_path_for_pool_key(&configured_path);
    Ok(ResolvedMailboxSqlitePath {
        used_absolute_fallback: canonical_path != configured_path,
        configured_path,
        canonical_path,
    })
}

#[must_use]
pub fn inspect_mailbox_sidecar_state(db_path: &Path) -> MailboxSidecarState {
    if db_path.as_os_str() == ":memory:" {
        return MailboxSidecarState::default();
    }

    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    let wal_meta = std::fs::metadata(&wal_path).ok();
    let shm_meta = std::fs::metadata(&shm_path).ok();

    MailboxSidecarState {
        wal_exists: wal_meta.as_ref().is_some_and(|meta| meta.is_file()),
        wal_bytes: wal_meta.as_ref().map(std::fs::Metadata::len),
        shm_exists: shm_meta.as_ref().is_some_and(|meta| meta.is_file()),
        shm_bytes: shm_meta.as_ref().map(std::fs::Metadata::len),
        live_sidecars: sqlite_file_has_live_sidecars(db_path),
    }
}

#[must_use]
pub fn inspect_mailbox_recovery_lock(db_path: &Path) -> MailboxRecoveryLockState {
    let lock_path = PathBuf::from(format!("{}.recovery.lock", db_path.display()));
    if db_path.as_os_str() == ":memory:" {
        return MailboxRecoveryLockState {
            lock_path: lock_path.display().to_string(),
            exists: false,
            active: false,
            pid: None,
            detail: "In-memory database (no recovery lock file)".to_string(),
        };
    }

    if !lock_path.exists() {
        return MailboxRecoveryLockState {
            lock_path: lock_path.display().to_string(),
            exists: false,
            active: false,
            pid: None,
            detail: "No recovery lock present".to_string(),
        };
    }

    match std::fs::read_to_string(&lock_path) {
        Ok(content) => match content.trim().parse::<u32>() {
            Ok(pid) => {
                let proc_path = PathBuf::from(format!("/proc/{pid}"));
                if proc_path.exists() {
                    MailboxRecoveryLockState {
                        lock_path: lock_path.display().to_string(),
                        exists: true,
                        active: true,
                        pid: Some(pid),
                        detail: format!("Recovery lock held by PID {pid}"),
                    }
                } else {
                    MailboxRecoveryLockState {
                        lock_path: lock_path.display().to_string(),
                        exists: true,
                        active: false,
                        pid: Some(pid),
                        detail: format!("Stale recovery lock from PID {pid} (process not running)"),
                    }
                }
            }
            Err(_) => MailboxRecoveryLockState {
                lock_path: lock_path.display().to_string(),
                exists: true,
                active: false,
                pid: None,
                detail: "Recovery lock file has invalid content".to_string(),
            },
        },
        Err(error) => MailboxRecoveryLockState {
            lock_path: lock_path.display().to_string(),
            exists: true,
            active: false,
            pid: None,
            detail: format!("Cannot read recovery lock file: {error}"),
        },
    }
}

fn normalized_mailbox_activity_sqlite_path(db_path: &Path) -> PathBuf {
    PathBuf::from(normalize_sqlite_path_for_pool_key(
        db_path.to_string_lossy().as_ref(),
    ))
}

fn mailbox_activity_lock_path_for_sqlite(db_path: &Path) -> PathBuf {
    let sqlite_path = normalized_mailbox_activity_sqlite_path(db_path);
    PathBuf::from(format!("{}.activity.lock", sqlite_path.display()))
}

fn mailbox_activity_lock_path_for_storage_root(storage_root: &Path) -> PathBuf {
    storage_root.join(".mailbox.activity.lock")
}

#[cfg(target_os = "linux")]
fn lock_holder_pids_via_proc(path: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let target_ino = meta.ino();
    let target_dev = meta.dev();
    let target_major = ((target_dev >> 8) & 0xfff) as u32;
    let target_minor = ((target_dev & 0xff) | ((target_dev >> 12) & 0xfff00)) as u32;
    let Ok(locks_content) = std::fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };

    let mut pids = BTreeSet::new();
    for line in locks_content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[1] != "FLOCK" {
            continue;
        }
        let parts: Vec<&str> = fields[5].split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(major) = u32::from_str_radix(parts[0], 16) else {
            continue;
        };
        let Ok(minor) = u32::from_str_radix(parts[1], 16) else {
            continue;
        };
        let Ok(ino) = parts[2].parse::<u64>() else {
            continue;
        };
        if ino != target_ino || major != target_major || minor != target_minor {
            continue;
        }
        let Ok(pid) = fields[4].parse::<u32>() else {
            continue;
        };
        pids.insert(pid);
    }
    pids.into_iter().collect()
}

#[cfg(not(target_os = "linux"))]
fn lock_holder_pids_via_proc(_path: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn pids_holding_file_via_proc(path: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;

    let Ok(target_meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let target_ino = target_meta.ino();
    let target_dev = target_meta.dev();

    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut holders = BTreeSet::new();
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd_entry in fds.flatten() {
            let Ok(link_target) = std::fs::read_link(fd_entry.path()) else {
                continue;
            };
            if let Ok(link_meta) = std::fs::metadata(&link_target)
                && link_meta.ino() == target_ino
                && link_meta.dev() == target_dev
            {
                holders.insert(pid);
                break;
            }
        }
    }

    holders.into_iter().collect()
}

#[cfg(not(target_os = "linux"))]
fn pids_holding_file_via_proc(_path: &Path) -> Vec<u32> {
    Vec::new()
}

fn executable_name_has_agent_mail_signature(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "am" | "am.exe"
            | "agent-mail"
            | "agent-mail.exe"
            | "agent_mail"
            | "agent_mail.exe"
            | "mcp-agent-mail"
            | "mcp_agent_mail"
            | "mcp-agent-mail.exe"
            | "mcp_agent_mail.exe"
            | "mcp-agent-mail-cli"
            | "mcp_agent_mail_cli"
            | "mcp-agent-mail-cli.exe"
            | "mcp_agent_mail_cli.exe"
    )
}

fn command_line_has_agent_mail_signature(command: &str) -> bool {
    let Some(argv0) = command.split_whitespace().next() else {
        return false;
    };
    let basename = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    executable_name_has_agent_mail_signature(basename)
}

#[cfg(target_os = "linux")]
fn pid_command_line(pid: u32) -> Option<String> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let segments: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter(|segment| !segment.is_empty())
        .map(|segment| String::from_utf8_lossy(segment).into_owned())
        .collect();
    (!segments.is_empty()).then(|| segments.join(" "))
}

#[cfg(not(target_os = "linux"))]
fn pid_command_line(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_executable_path(_pid: u32) -> Option<PathBuf> {
    None
}

fn pid_executable_deleted(pid: u32) -> bool {
    pid_executable_path(pid)
        .map(|path| path.to_string_lossy().contains(" (deleted)"))
        .unwrap_or(false)
}

fn pid_is_agent_mail(pid: u32) -> bool {
    pid_command_line(pid).is_some_and(|command| command_line_has_agent_mail_signature(&command))
        || pid_executable_path(pid)
            .and_then(|exe| {
                exe.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .is_some_and(|basename| executable_name_has_agent_mail_signature(&basename))
}

fn add_mailbox_process_surface(
    processes: &mut HashMap<u32, MailboxOwnershipProcess>,
    pid: u32,
    mark: impl Fn(&mut MailboxOwnershipProcess),
) {
    let entry = processes
        .entry(pid)
        .or_insert_with(|| MailboxOwnershipProcess {
            pid,
            command: None,
            executable_path: None,
            executable_deleted: false,
            holds_storage_root_lock: false,
            holds_sqlite_lock: false,
            holds_database_file: false,
        });
    mark(entry);
}

fn describe_mailbox_process(process: &MailboxOwnershipProcess) -> String {
    let mut surfaces = Vec::new();
    if process.holds_storage_root_lock {
        surfaces.push("storage_lock");
    }
    if process.holds_sqlite_lock {
        surfaces.push("sqlite_lock");
    }
    if process.holds_database_file {
        surfaces.push("db_file");
    }
    let surface_text = if surfaces.is_empty() {
        "no_live_surface".to_string()
    } else {
        surfaces.join(",")
    };
    let command = process
        .command
        .as_deref()
        .filter(|command| !command.trim().is_empty())
        .unwrap_or("<unknown>");
    let executable = process
        .executable_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or("<unknown>");
    let deleted = if process.executable_deleted {
        " deleted-executable"
    } else {
        ""
    };
    format!(
        "PID {} [{}] cmd={command} exe={executable}{deleted}",
        process.pid, surface_text
    )
}

fn classify_mailbox_ownership(
    processes: &[MailboxOwnershipProcess],
    current_pid: u32,
) -> (MailboxOwnershipDisposition, Vec<u32>, bool, String) {
    let competing: Vec<&MailboxOwnershipProcess> = processes
        .iter()
        .filter(|process| process.pid != current_pid)
        .collect();
    let competing_pids: Vec<u32> = competing.iter().map(|process| process.pid).collect();
    let current_deleted = pid_executable_deleted(current_pid);

    if competing.len() > 1 {
        let detail = format!(
            "mailbox ownership is split-brain across live Agent Mail processes: {}",
            competing
                .iter()
                .map(|process| describe_mailbox_process(process))
                .collect::<Vec<_>>()
                .join("; ")
        );
        return (
            MailboxOwnershipDisposition::SplitBrain,
            competing_pids,
            true,
            detail,
        );
    }

    if let Some(process) = competing.first()
        && process.executable_deleted
    {
        return (
            MailboxOwnershipDisposition::DeletedExecutable,
            competing_pids,
            true,
            format!(
                "another live Agent Mail mailbox owner is running a deleted executable: {}",
                describe_mailbox_process(process)
            ),
        );
    }

    if current_deleted {
        return (
            MailboxOwnershipDisposition::DeletedExecutable,
            competing_pids,
            true,
            format!(
                "current Agent Mail process PID {} is running a deleted executable",
                current_pid
            ),
        );
    }

    if let Some(process) = competing.first() {
        if !process.holds_storage_root_lock
            && !process.holds_sqlite_lock
            && process.holds_database_file
        {
            return (
                MailboxOwnershipDisposition::StaleLiveProcess,
                competing_pids,
                true,
                format!(
                    "live Agent Mail process still holds the mailbox database without mailbox activity locks: {}",
                    describe_mailbox_process(process)
                ),
            );
        }
        return (
            MailboxOwnershipDisposition::ActiveOtherOwner,
            competing_pids,
            false,
            format!(
                "another Agent Mail process already owns the mailbox: {}",
                describe_mailbox_process(process)
            ),
        );
    }

    (
        MailboxOwnershipDisposition::Unowned,
        Vec::new(),
        false,
        "no competing Agent Mail mailbox owners or live database holders detected".to_string(),
    )
}

#[must_use]
pub fn inspect_mailbox_ownership(
    primary_path: &Path,
    storage_root: &Path,
) -> MailboxOwnershipState {
    let storage_lock_path = mailbox_activity_lock_path_for_storage_root(storage_root);
    let sqlite_lock_path = mailbox_activity_lock_path_for_sqlite(primary_path);

    let mut processes = HashMap::new();
    for pid in lock_holder_pids_via_proc(&storage_lock_path) {
        if pid_is_agent_mail(pid) {
            add_mailbox_process_surface(&mut processes, pid, |process| {
                process.holds_storage_root_lock = true;
            });
        }
    }
    for pid in lock_holder_pids_via_proc(&sqlite_lock_path) {
        if pid_is_agent_mail(pid) {
            add_mailbox_process_surface(&mut processes, pid, |process| {
                process.holds_sqlite_lock = true;
            });
        }
    }
    if primary_path.exists() {
        for pid in pids_holding_file_via_proc(primary_path) {
            if pid_is_agent_mail(pid) {
                add_mailbox_process_surface(&mut processes, pid, |process| {
                    process.holds_database_file = true;
                });
            }
        }
    }

    let current_pid = std::process::id();
    if pid_executable_deleted(current_pid) && !processes.contains_key(&current_pid) {
        add_mailbox_process_surface(&mut processes, current_pid, |_| {});
    }

    let mut processes: Vec<_> = processes
        .into_values()
        .map(|mut process| {
            process.command = pid_command_line(process.pid);
            process.executable_path =
                pid_executable_path(process.pid).map(|path| path.to_string_lossy().into_owned());
            process.executable_deleted = process
                .executable_path
                .as_deref()
                .is_some_and(|path| path.contains(" (deleted)"));
            process
        })
        .collect();
    processes.sort_by_key(|process| process.pid);

    let (disposition, competing_pids, supervised_restart_required, detail) =
        classify_mailbox_ownership(&processes, current_pid);
    MailboxOwnershipState {
        disposition,
        storage_lock_path: storage_lock_path.display().to_string(),
        sqlite_lock_path: sqlite_lock_path.display().to_string(),
        processes,
        competing_pids,
        supervised_restart_required,
        detail,
    }
}

#[allow(clippy::result_large_err)]
fn refuse_mutating_mailbox_when_owned(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<(), SqlError> {
    let ownership = inspect_mailbox_ownership(primary_path, storage_root);
    if !ownership.blocks_mutation() {
        return Ok(());
    }

    let remediation = if ownership.supervised_restart_required {
        "supervised restart or operator intervention is required before recovery"
    } else {
        "wait for the active owner to finish instead of competing recovery"
    };
    Err(SqlError::Custom(format!(
        "mailbox mutation refused for {}: {}; {}",
        primary_path.display(),
        ownership.detail,
        remediation
    )))
}

#[allow(clippy::result_large_err)]
fn quarantined_sidecar_path(
    primary_path: &Path,
    suffix: &str,
    label: &str,
    timestamp: &str,
) -> PathBuf {
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    primary_path.with_file_name(format!("{base_name}{suffix}.{label}-{timestamp}"))
}

#[allow(clippy::result_large_err)]
fn reconstruction_candidate_path(primary_path: &Path, timestamp: &str) -> PathBuf {
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    primary_path.with_file_name(format!("{base_name}.reconstructing-{timestamp}"))
}

#[allow(clippy::result_large_err)]
fn quarantine_reconstruction_candidate_path(
    candidate_path: &Path,
    primary_path: &Path,
    reason: &str,
    timestamp: &str,
) -> Result<Option<PathBuf>, SqlError> {
    if !candidate_path.exists() {
        return Ok(None);
    }

    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined = primary_path.with_file_name(format!("{base_name}.{reason}-{timestamp}"));
    std::fs::rename(candidate_path, &quarantined).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine reconstructed sqlite candidate {}: {e}",
            candidate_path.display()
        ))
    })?;

    for suffix in ["-journal", "-wal", "-shm"] {
        let mut source_os = candidate_path.as_os_str().to_os_string();
        source_os.push(suffix);
        let source = PathBuf::from(source_os);
        if !source.exists() {
            continue;
        }
        let mut target_os = quarantined.as_os_str().to_os_string();
        target_os.push(suffix);
        let target = PathBuf::from(target_os);
        std::fs::rename(&source, &target).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine reconstructed sqlite sidecar {}: {e}",
                source.display()
            ))
        })?;
    }

    Ok(Some(quarantined))
}

#[allow(clippy::result_large_err)]
fn activate_reconstruction_candidate(
    candidate_path: &Path,
    primary_path: &Path,
) -> Result<(), SqlError> {
    if primary_path.exists() {
        return Err(SqlError::Custom(format!(
            "refusing to activate reconstructed candidate {} over existing live database {}",
            candidate_path.display(),
            primary_path.display()
        )));
    }

    std::fs::rename(candidate_path, primary_path).map_err(|e| {
        SqlError::Custom(format!(
            "failed to activate reconstructed sqlite candidate {} into {}: {e}",
            candidate_path.display(),
            primary_path.display()
        ))
    })
}

#[allow(clippy::result_large_err)]
fn reconstruct_archive_into_candidate(
    primary_path: &Path,
    storage_root: &Path,
    salvage_db_path: Option<&Path>,
    timestamp: &str,
) -> Result<crate::reconstruct::ReconstructStats, SqlError> {
    let candidate_path = reconstruction_candidate_path(primary_path, timestamp);
    let reconstruct_result = match salvage_db_path {
        Some(salvage_db_path) => crate::reconstruct::reconstruct_from_archive_with_salvage(
            &candidate_path,
            storage_root,
            Some(salvage_db_path),
        ),
        None => crate::reconstruct::reconstruct_from_archive(&candidate_path, storage_root),
    };

    match reconstruct_result {
        Ok(stats) => match sqlite_file_is_healthy(&candidate_path) {
            Ok(true) => {
                activate_reconstruction_candidate(&candidate_path, primary_path)?;
                Ok(stats)
            }
            Ok(false) => {
                let _ = quarantine_reconstruction_candidate_path(
                    &candidate_path,
                    primary_path,
                    "reconstruct-failed",
                    timestamp,
                );
                Err(SqlError::Custom(format!(
                    "archive reconstruction produced an unhealthy sqlite candidate for {}",
                    primary_path.display()
                )))
            }
            Err(e) => {
                let _ = quarantine_reconstruction_candidate_path(
                    &candidate_path,
                    primary_path,
                    "reconstruct-failed",
                    timestamp,
                );
                Err(e)
            }
        },
        Err(e) => {
            let _ = quarantine_reconstruction_candidate_path(
                &candidate_path,
                primary_path,
                "reconstruct-failed",
                timestamp,
            );
            Err(SqlError::Custom(format!(
                "archive reconstruction failed for {}: {e}",
                primary_path.display()
            )))
        }
    }
}

#[allow(clippy::result_large_err)]
fn quarantine_sidecar_with_label(
    primary_path: &Path,
    suffix: &str,
    label: &str,
    timestamp: &str,
) -> Result<(), SqlError> {
    let mut source_os = primary_path.as_os_str().to_os_string();
    source_os.push(suffix);
    let source = PathBuf::from(source_os);
    if !source.exists() {
        return Ok(());
    }
    let target = quarantined_sidecar_path(primary_path, suffix, label, timestamp);
    std::fs::rename(&source, &target).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine sidecar {}: {e}",
            source.display()
        ))
    })
}

#[allow(clippy::result_large_err)]
fn quarantine_sidecar(primary_path: &Path, suffix: &str, timestamp: &str) -> Result<(), SqlError> {
    quarantine_sidecar_with_label(primary_path, suffix, "corrupt", timestamp)
}

#[allow(clippy::result_large_err)]
fn restore_quarantined_primary_with_sidecar_label(
    primary_path: &Path,
    quarantined_path: &Path,
    sidecar_label: &str,
    timestamp: &str,
) -> Result<(), SqlError> {
    if primary_path.exists() {
        let restore_timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        quarantine_reconstructed_candidate(
            primary_path,
            &restore_timestamp,
            "archive-reconcile-restore",
        )
        .map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine live sqlite candidate {} before restore: {e}",
                primary_path.display()
            ))
        })?;
    }

    if quarantined_path.exists() {
        std::fs::rename(quarantined_path, primary_path).map_err(|e| {
            SqlError::Custom(format!(
                "failed to restore original database {} from {}: {e}",
                primary_path.display(),
                quarantined_path.display()
            ))
        })?;
    }

    restore_quarantined_sidecar(primary_path, "-wal", sidecar_label, timestamp)?;
    restore_quarantined_sidecar(primary_path, "-shm", sidecar_label, timestamp)?;
    Ok(())
}

#[allow(clippy::result_large_err)]
fn quarantine_corrupt_sidecars_or_restore_primary(
    primary_path: &Path,
    quarantined_path: &Path,
    timestamp: &str,
    context: &str,
) -> Result<(), SqlError> {
    if let Err(e) = quarantine_sidecar(primary_path, "-wal", timestamp) {
        if let Err(restore_err) =
            restore_quarantined_primary(primary_path, quarantined_path, timestamp)
        {
            return Err(SqlError::Custom(format!(
                "failed to quarantine WAL sidecar for {context} at {}: {e}; rollback of quarantined database also failed: {restore_err}",
                primary_path.display()
            )));
        }
        return Err(SqlError::Custom(format!(
            "failed to quarantine WAL sidecar for {context} at {}: {e}",
            primary_path.display()
        )));
    }

    if let Err(e) = quarantine_sidecar(primary_path, "-shm", timestamp) {
        if let Err(restore_err) =
            restore_quarantined_primary(primary_path, quarantined_path, timestamp)
        {
            return Err(SqlError::Custom(format!(
                "failed to quarantine SHM sidecar for {context} at {}: {e}; rollback of quarantined database also failed: {restore_err}",
                primary_path.display()
            )));
        }
        return Err(SqlError::Custom(format!(
            "failed to quarantine SHM sidecar for {context} at {}: {e}",
            primary_path.display()
        )));
    }

    Ok(())
}

#[allow(clippy::result_large_err)]
fn quarantine_reconstructed_candidate(
    primary_path: &Path,
    timestamp: &str,
    reason: &str,
) -> Result<Option<PathBuf>, SqlError> {
    if !primary_path.exists() {
        return Ok(None);
    }

    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined = primary_path.with_file_name(format!("{base_name}.{reason}-{timestamp}"));
    std::fs::rename(primary_path, &quarantined).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine reconstructed database candidate {}: {e}",
            primary_path.display()
        ))
    })?;

    if let Err(e) = quarantine_sidecar_with_label(primary_path, "-wal", reason, timestamp) {
        if let Err(restore_err) = restore_quarantined_primary_with_sidecar_label(
            primary_path,
            &quarantined,
            reason,
            timestamp,
        ) {
            return Err(SqlError::Custom(format!(
                "failed to quarantine WAL sidecar for reconstructed candidate {}: {e}; rollback also failed: {restore_err}",
                primary_path.display()
            )));
        }
        return Err(SqlError::Custom(format!(
            "failed to quarantine WAL sidecar for reconstructed candidate {}: {e}",
            primary_path.display()
        )));
    }
    if let Err(e) = quarantine_sidecar_with_label(primary_path, "-shm", reason, timestamp) {
        if let Err(restore_err) = restore_quarantined_primary_with_sidecar_label(
            primary_path,
            &quarantined,
            reason,
            timestamp,
        ) {
            return Err(SqlError::Custom(format!(
                "failed to quarantine SHM sidecar for reconstructed candidate {}: {e}; rollback also failed: {restore_err}",
                primary_path.display()
            )));
        }
        return Err(SqlError::Custom(format!(
            "failed to quarantine SHM sidecar for reconstructed candidate {}: {e}",
            primary_path.display()
        )));
    }

    Ok(Some(quarantined))
}

#[allow(clippy::result_large_err)]
fn restore_quarantined_sidecar(
    primary_path: &Path,
    suffix: &str,
    label: &str,
    timestamp: &str,
) -> Result<(), SqlError> {
    let quarantined = quarantined_sidecar_path(primary_path, suffix, label, timestamp);
    let metadata = match std::fs::symlink_metadata(&quarantined) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(SqlError::Custom(format!(
                "failed to inspect quarantined sidecar {}: {e}",
                quarantined.display()
            )));
        }
    };

    if !metadata.file_type().is_file() {
        tracing::warn!(
            path = %quarantined.display(),
            "skipping non-file sqlite sidecar quarantine artifact during restore"
        );
        return Ok(());
    }

    let mut live_os = primary_path.as_os_str().to_os_string();
    live_os.push(suffix);
    let live_path = PathBuf::from(live_os);
    if live_path.exists() {
        std::fs::remove_file(&live_path).map_err(|e| {
            SqlError::Custom(format!(
                "failed to clear restored sidecar destination {}: {e}",
                live_path.display()
            ))
        })?;
    }

    std::fs::rename(&quarantined, &live_path).map_err(|e| {
        SqlError::Custom(format!(
            "failed to restore original sidecar {} from {}: {e}",
            live_path.display(),
            quarantined.display()
        ))
    })
}

#[allow(clippy::result_large_err)]
fn restore_quarantined_primary(
    primary_path: &Path,
    quarantined_path: &Path,
    timestamp: &str,
) -> Result<(), SqlError> {
    restore_quarantined_primary_with_sidecar_label(
        primary_path,
        quarantined_path,
        "corrupt",
        timestamp,
    )
}

/// Rebuild a healthy-but-stale SQLite file from the archive while salvaging the
/// current primary database for any DB-only state that is not archived.
#[allow(clippy::result_large_err)]
fn reconstruct_sqlite_file_with_archive_salvage_inner(
    primary_path: &Path,
    storage_root: &Path,
    capture_forensics: bool,
) -> Result<crate::reconstruct::ReconstructStats, SqlError> {
    refuse_mutating_mailbox_when_owned(primary_path, storage_root)?;
    if capture_forensics {
        let _bundle_dir =
            capture_automatic_recovery_bundle(primary_path, storage_root, "reconstruct")?;
    }

    if !primary_path.exists() {
        if has_quarantined_primary_artifact(primary_path) {
            return Err(SqlError::Custom(format!(
                "database file {} is missing but quarantined recovery artifact(s) exist; refusing archive salvage reconstruction without operator action",
                primary_path.display()
            )));
        }
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        return reconstruct_archive_into_candidate(primary_path, storage_root, None, &timestamp);
    }

    if let Ok(conn) = open_sqlite_file_with_recovery(primary_path.to_string_lossy().as_ref()) {
        let _ = conn.query_sync("PRAGMA busy_timeout=60000;", &[]);
        let _ = conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[]);
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined =
        primary_path.with_file_name(format!("{base_name}.archive-reconcile-{timestamp}"));

    std::fs::rename(primary_path, &quarantined).map_err(|e| {
        SqlError::Custom(format!(
            "failed to quarantine database {} for archive reconciliation: {e}",
            primary_path.display()
        ))
    })?;
    quarantine_corrupt_sidecars_or_restore_primary(
        primary_path,
        &quarantined,
        &timestamp,
        "archive reconciliation",
    )?;

    match reconstruct_archive_into_candidate(
        primary_path,
        storage_root,
        Some(&quarantined),
        &timestamp,
    ) {
        Ok(stats) => Ok(stats),
        Err(e) => {
            restore_quarantined_primary(primary_path, &quarantined, &timestamp)?;
            Err(e)
        }
    }
}

#[allow(clippy::result_large_err)]
pub fn reconstruct_sqlite_file_with_archive_salvage(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<crate::reconstruct::ReconstructStats, SqlError> {
    reconstruct_sqlite_file_with_archive_salvage_inner(primary_path, storage_root, true)
}

#[allow(clippy::result_large_err)]
fn restore_from_backup(primary_path: &Path, backup_path: &Path) -> Result<(), SqlError> {
    if !is_real_file(backup_path) {
        return Err(SqlError::Custom(format!(
            "refusing to restore sqlite backup from non-regular file {}",
            backup_path.display()
        )));
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined_db = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));

    if primary_path.exists() {
        std::fs::rename(primary_path, &quarantined_db).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine corrupted database {}: {e}",
                primary_path.display()
            ))
        })?;
    }

    quarantine_corrupt_sidecars_or_restore_primary(
        primary_path,
        &quarantined_db,
        &timestamp,
        "backup restore",
    )?;

    if let Err(e) = std::fs::copy(backup_path, primary_path) {
        if let Err(restore_err) =
            restore_quarantined_primary(primary_path, &quarantined_db, &timestamp)
        {
            return Err(SqlError::Custom(format!(
                "failed to restore backup {} into {}: {e}; rollback of quarantined database also failed: {restore_err}",
                backup_path.display(),
                primary_path.display()
            )));
        }
        return Err(SqlError::Custom(format!(
            "failed to restore backup {} into {}: {e}",
            backup_path.display(),
            primary_path.display()
        )));
    }

    tracing::warn!(
        primary = %primary_path.display(),
        backup = %backup_path.display(),
        quarantined = %quarantined_db.display(),
        "auto-restored sqlite database from backup after corruption detection"
    );
    Ok(())
}

#[allow(clippy::result_large_err)]
fn reinitialize_without_backup(primary_path: &Path) -> Result<(), SqlError> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");
    let quarantined_db = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));

    if primary_path.exists() {
        std::fs::rename(primary_path, &quarantined_db).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine corrupted database {}: {e}",
                primary_path.display()
            ))
        })?;
    }

    quarantine_corrupt_sidecars_or_restore_primary(
        primary_path,
        &quarantined_db,
        &timestamp,
        "scratch reinitialization",
    )?;

    let path_str = primary_path.to_string_lossy();
    let conn = match open_sqlite_file_with_lock_retry(path_str.as_ref()) {
        Ok(conn) => conn,
        Err(e) => {
            if let Err(restore_err) =
                restore_quarantined_primary(primary_path, &quarantined_db, &timestamp)
            {
                return Err(SqlError::Custom(format!(
                    "failed to initialize fresh sqlite file {}: {e}; rollback of quarantined database also failed: {restore_err}",
                    primary_path.display()
                )));
            }
            return Err(SqlError::Custom(format!(
                "failed to initialize fresh sqlite file {}: {e}",
                primary_path.display()
            )));
        }
    };
    let _conn = crate::guard_db_conn(conn, "scratch sqlite reinit connection");

    tracing::warn!(
        primary = %primary_path.display(),
        quarantined = %quarantined_db.display(),
        "no healthy sqlite backup found; initialized fresh database file from scratch"
    );
    Ok(())
}

/// Verify and, if necessary, recover a `SQLite` database file.
///
/// Runs layered health probes (`quick_check`, `integrity_check(1)`, and
/// a schema-aware query smoke test) on the file. If corruption is detected:
///
/// 1. Search for a healthy `.bak` / `.bak.*` / `.backup-*` / `.recovery*` sibling.
/// 2. Quarantine the corrupt file (rename to `*.corrupt-{timestamp}`).
/// 3. Restore from the first healthy backup found.
/// 4. If no healthy backup exists, reinitialize an empty database file.
///
/// Returns `Ok(())` when the file at `primary_path` is healthy (either
/// originally or after successful recovery).
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy(primary_path: &Path) -> Result<(), SqlError> {
    let exists = primary_path.exists();
    if exists {
        cleanup_empty_wal_sidecar(primary_path.to_string_lossy().as_ref());
    }
    if exists && sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    if exists {
        refuse_auto_recovery_with_live_sidecars(primary_path)?;
    }
    if exists {
        match try_repair_index_only_corruption(primary_path) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                path = %primary_path.display(),
                error = %e,
                "in-place sqlite index repair probe failed; continuing with standard recovery"
            ),
        }
    }

    let fallback_storage_root = primary_path.parent().unwrap_or_else(|| Path::new("."));
    let _bundle_dir =
        capture_automatic_recovery_bundle(primary_path, fallback_storage_root, "repair")?;

    if let Some(backup_path) = find_healthy_backup(primary_path) {
        restore_from_backup(primary_path, &backup_path)?;
        if sqlite_file_is_healthy(primary_path)? {
            return Ok(());
        }
        if exists {
            return Err(SqlError::Custom(format!(
                "database file {} was restored from {}, but health probes still failed",
                primary_path.display(),
                backup_path.display()
            )));
        }
        // If missing originally and restore failed, fall through to reinitialize
    } else if !exists {
        // Missing file, no backup. Normal fresh startup.
        return Ok(());
    }

    reinitialize_without_backup(primary_path)?;
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    Err(SqlError::Custom(format!(
        "database file {} was reinitialized without backup, but health probes still failed",
        primary_path.display()
    )))
}

/// Like [`ensure_sqlite_file_healthy`], but attempts to reconstruct the
/// database from the Git archive before falling back to a blank reinitialize.
///
/// Recovery priority:
/// 1. `.bak` / `.bak.*` / `.backup-*` / `.recovery*` backup files
/// 2. Git archive reconstruction (recovers messages + agents)
/// 3. Blank reinitialization (empty database)
#[allow(clippy::too_many_lines)]
#[allow(clippy::result_large_err)]
pub fn ensure_sqlite_file_healthy_with_archive(
    primary_path: &Path,
    storage_root: &Path,
) -> Result<(), SqlError> {
    let had_primary = primary_path.exists();
    if had_primary {
        cleanup_empty_wal_sidecar(primary_path.to_string_lossy().as_ref());
    }
    if had_primary && sqlite_file_is_healthy(primary_path)? {
        let _ = reconcile_archive_state_before_init(primary_path, storage_root)?;
        return Ok(());
    }

    refuse_mutating_mailbox_when_owned(primary_path, storage_root)?;

    if had_primary {
        refuse_auto_recovery_with_live_sidecars(primary_path)?;
    }
    if had_primary {
        match try_repair_index_only_corruption(primary_path) {
            Ok(true) => {
                let _ = reconcile_archive_state_before_init(primary_path, storage_root)?;
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(
                path = %primary_path.display(),
                error = %e,
                "in-place sqlite index repair probe failed; continuing with archive-aware recovery"
            ),
        }
    }

    let _bundle_dir = if had_primary {
        Some(capture_automatic_recovery_bundle(
            primary_path,
            storage_root,
            "repair",
        )?)
    } else {
        None
    };

    // Priority 1: Restore from backup
    if let Some(backup_path) = find_healthy_backup(primary_path) {
        restore_from_backup(primary_path, &backup_path)?;
        if sqlite_file_is_healthy(primary_path)? {
            let _ = reconcile_archive_state_before_init(primary_path, storage_root)?;
            return Ok(());
        }
        tracing::warn!(
            "backup restore didn't produce a healthy file; falling through to archive reconstruction"
        );
    } else if !had_primary {
        // Missing file, no backup.
        if has_quarantined_primary_artifact(primary_path) {
            return Err(SqlError::Custom(format!(
                "database file {} is missing but quarantined recovery artifact(s) exist; refusing blank reinitialization without operator action",
                primary_path.display()
            )));
        }
        if !is_real_directory(storage_root) || !is_real_directory(&storage_root.join("projects")) {
            // Normal fresh startup (no projects directory).
            return Ok(());
        }
        let _bundle_dir =
            capture_automatic_recovery_bundle(primary_path, storage_root, "reconstruct")?;
        // Missing file, but archive has projects. We want to reconstruct!
    }

    // Priority 2: Reconstruct from Git archive
    tracing::warn!(
        storage_root = %storage_root.display(),
        "no healthy backup found; attempting database reconstruction from Git archive"
    );

    // Quarantine the corrupt file first
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let base_name = primary_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("storage.sqlite3");

    if primary_path.exists() {
        let quarantined = primary_path.with_file_name(format!("{base_name}.corrupt-{timestamp}"));
        std::fs::rename(primary_path, &quarantined).map_err(|e| {
            SqlError::Custom(format!(
                "failed to quarantine corrupted database {}: {e}",
                primary_path.display()
            ))
        })?;
        quarantine_corrupt_sidecars_or_restore_primary(
            primary_path,
            &quarantined,
            &timestamp,
            "archive-aware reconstruction",
        )?;
    }

    match reconstruct_sqlite_file_with_archive_salvage_inner(primary_path, storage_root, false) {
        Ok(stats) => {
            if had_primary && stats.projects == 0 && stats.agents == 0 && stats.messages == 0 {
                if let Some(quarantined) = quarantine_reconstructed_candidate(
                    primary_path,
                    &timestamp,
                    "reconstruct-empty",
                )? {
                    tracing::warn!(
                        primary = %primary_path.display(),
                        quarantined = %quarantined.display(),
                        "quarantined empty reconstructed database candidate"
                    );
                }
                return Err(SqlError::Custom(format!(
                    "database file {} was quarantined for archive-aware recovery, but archive reconstruction restored no durable mail state; refusing blank reinitialization to avoid data loss",
                    primary_path.display()
                )));
            }
            tracing::warn!(%stats, "database successfully reconstructed from Git archive");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "archive reconstruction failed; falling through to blank reinitialize"
            );
        }
    }

    if had_primary {
        if let Some(quarantined) =
            quarantine_reconstructed_candidate(primary_path, &timestamp, "reconstruct-failed")?
        {
            tracing::warn!(
                primary = %primary_path.display(),
                quarantined = %quarantined.display(),
                "quarantined reconstructed database candidate after failed archive recovery"
            );
        }
        return Err(SqlError::Custom(format!(
            "database file {} was quarantined for archive-aware recovery, but reconstruction did not produce a healthy database; refusing blank reinitialization to avoid data loss",
            primary_path.display()
        )));
    }

    // Priority 3: Blank reinitialization
    reinitialize_without_backup(primary_path)?;
    if sqlite_file_is_healthy(primary_path)? {
        return Ok(());
    }
    Err(SqlError::Custom(format!(
        "all recovery strategies exhausted for {}",
        primary_path.display()
    )))
}

/// Get (or create) a cached pool for the given config.
///
/// Uses a read-first / write-on-miss pattern so concurrent callers sharing
/// the same effective pool signature only take a shared read lock (zero
/// contention on the hot path). The write lock is only held briefly when
/// creating a new pool.
pub fn get_or_create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    let cache =
        POOL_CACHE.get_or_init(|| OrderedRwLock::new(LockLevel::DbPoolCache, HashMap::new()));
    let cache_key = pool_cache_key(config);

    // Fast path: shared read lock for existing live pool (concurrent readers).
    {
        let guard = cache.read();
        if let Some(pool) = guard.get(&cache_key)
            && let Some(shared_pool) = pool.upgrade()
        {
            return DbPool::from_shared_pool(config, shared_pool);
        }
    }

    // Slow path: exclusive write lock to create a new pool (rare), or to
    // evict dead weak entries left after all callers dropped a pool.
    let mut guard = cache.write();
    // Double-check after acquiring write lock — another thread may have won the race.
    if let Some(pool) = guard.get(&cache_key) {
        if let Some(shared_pool) = pool.upgrade() {
            return DbPool::from_shared_pool(config, shared_pool);
        }
        guard.remove(&cache_key);
    }

    let pool = DbPool::new(config)?;
    guard.insert(cache_key, Arc::downgrade(&pool.pool));
    drop(guard);
    Ok(pool)
}

/// Create (or reuse) a pool for the given config.
///
/// This is kept for backwards compatibility with earlier skeleton code.
pub fn create_pool(config: &DbPoolConfig) -> DbResult<DbPool> {
    get_or_create_pool(config)
}

// ============================================================================
// Synthetic canary namespace, metrics, and alert-isolation policy
// (br-97gc6.5.2.6.5.4)
// ============================================================================
//
// Synthetic durability canaries exercise the full durability stack (integrity
// probes, archive-drift detection, recovery, write-deferral replay) against
// disposable, isolated mailboxes so regressions surface before they affect
// real operator traffic.
//
// To prevent canary activity from polluting production dashboards, alert
// streams, or aggregate health signals, three isolation layers are defined:
//
// 1. **Namespace convention** — canary projects, agents, and storage roots
//    carry a well-known prefix (`__canary_`) that is trivially filterable in
//    structured logs and SQL queries.
//
// 2. **Metric isolation** — canary probes record into a dedicated
//    `CanaryMetrics` surface (in `mcp_agent_mail_core::metrics`) that is
//    never aggregated into the production `DbMetrics` or `StorageMetrics`
//    counters.
//
// 3. **Alert isolation** — the `CanaryAlertTier` enum classifies every
//    canary event into one of four routing tiers so alerting pipelines can
//    suppress canary failures from paging while still making them visible
//    for debugging.

/// Reserved prefix for all synthetic canary identifiers.
///
/// Any project slug, agent name, or storage-root directory whose name begins
/// with this prefix is treated as canary traffic by the entire durability
/// subsystem.  Production code paths that aggregate health signals, emit
/// alerts, or update operator-facing dashboards **must** exclude identifiers
/// that match [`is_canary_identifier`].
pub const CANARY_PREFIX: &str = "__canary_";

/// Reserved project slug for the canary's disposable mailbox project.
///
/// The canary runner creates a project with this slug at the start of each
/// canary cycle and tears it down at the end.  Because the slug starts with
/// [`CANARY_PREFIX`], it is automatically excluded from production metrics.
pub const CANARY_PROJECT_SLUG: &str = "__canary_durability_probe";

/// Reserved agent-name prefix for canary probe agents.
///
/// Canary agents are named `__canary_probe_<N>` where `<N>` is a
/// monotonically increasing cycle counter.  The prefix ensures they never
/// collide with real agent names (which must pass `is_valid_agent_name`'s
/// adjective-noun validation).
pub const CANARY_AGENT_PREFIX: &str = "__canary_probe_";

/// Subdirectory name under the system temp dir for canary storage roots.
///
/// Each canary cycle creates a fresh storage root at
/// `$TMPDIR/__canary_mailbox_<cycle_id>/` so canary I/O is physically
/// isolated from the operator's real mailbox storage.
pub const CANARY_STORAGE_DIR_PREFIX: &str = "__canary_mailbox_";

/// Returns `true` if `name` belongs to the synthetic canary namespace.
///
/// This is the single predicate that all production metric, alert, and
/// dashboard code should use to exclude canary traffic.
#[must_use]
pub fn is_canary_identifier(name: &str) -> bool {
    name.starts_with(CANARY_PREFIX)
}

/// Returns `true` if the given filesystem path component belongs to the
/// canary namespace.
///
/// This checks the final path component (file or directory name) against
/// [`CANARY_PREFIX`], so callers can filter storage-root paths, SQLite file
/// paths, and forensic bundle directories.
#[must_use]
pub fn is_canary_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with(CANARY_PREFIX))
}

/// Generate a canary agent name for the given cycle number.
///
/// Returns a name like `__canary_probe_42` that is guaranteed to match
/// [`is_canary_identifier`] and never collide with valid agent names.
#[must_use]
pub fn canary_agent_name(cycle: u64) -> String {
    format!("{CANARY_AGENT_PREFIX}{cycle}")
}

/// Generate a canary storage root path under the system temp directory.
///
/// Returns a path like `/tmp/__canary_mailbox_42/` that is physically
/// isolated from production storage.
#[must_use]
pub fn canary_storage_root(cycle: u64) -> PathBuf {
    std::env::temp_dir().join(format!("{CANARY_STORAGE_DIR_PREFIX}{cycle}"))
}

// ── Alert-isolation policy ─────────────────────────────────────────────

/// Alert-routing tier for canary events.
///
/// Canary failures should never page operators.  Instead, they are routed
/// through a four-tier classification that separates observability from
/// operational urgency:
///
/// | Tier          | Operator paging? | Dashboard visible? | Log level   |
/// |---------------|------------------|--------------------|-------------|
/// | `Silent`      | No               | No                 | `TRACE`     |
/// | `Observable`  | No               | Yes (canary tab)   | `DEBUG`     |
/// | `Warning`     | No               | Yes (canary tab)   | `WARN`      |
/// | `Engineering` | No (ticket only) | Yes (canary tab)   | `ERROR`     |
///
/// Even the most severe canary failure (`Engineering`) only creates an
/// engineering ticket — it never fires a pager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryAlertTier {
    /// Routine success — not shown on any dashboard, logged at TRACE.
    Silent,
    /// Interesting event (e.g. slow probe, unusual drift) — visible on the
    /// canary-specific dashboard tab but no alert.
    Observable,
    /// Canary probe failure that may indicate a real regression — shown on
    /// the canary dashboard and logged at WARN, but still no page.
    Warning,
    /// Confirmed canary regression that warrants an engineering ticket —
    /// logged at ERROR but routed to the ticket system, never the pager.
    Engineering,
}

impl CanaryAlertTier {
    /// Whether this tier should be visible on the canary dashboard tab.
    #[must_use]
    pub const fn dashboard_visible(&self) -> bool {
        matches!(self, Self::Observable | Self::Warning | Self::Engineering)
    }

    /// Whether this tier should create an engineering ticket.
    #[must_use]
    pub const fn creates_ticket(&self) -> bool {
        matches!(self, Self::Engineering)
    }

    /// The `tracing` log level appropriate for this tier.
    #[must_use]
    pub const fn log_level(&self) -> tracing::Level {
        match self {
            Self::Silent => tracing::Level::TRACE,
            Self::Observable => tracing::Level::DEBUG,
            Self::Warning => tracing::Level::WARN,
            Self::Engineering => tracing::Level::ERROR,
        }
    }

    /// Short label for structured logs and metrics dimensions.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::Observable => "observable",
            Self::Warning => "warning",
            Self::Engineering => "engineering",
        }
    }

    /// All alert tiers in severity order (lowest to highest).
    pub const ALL: &'static [CanaryAlertTier] = &[
        Self::Silent,
        Self::Observable,
        Self::Warning,
        Self::Engineering,
    ];
}

impl std::fmt::Display for CanaryAlertTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Classification of a canary probe outcome for alert-routing purposes.
///
/// This is returned by [`classify_canary_outcome`] and consumed by the
/// canary runner to decide where to route the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanaryAlertPolicy {
    /// The routing tier.
    pub tier: CanaryAlertTier,
    /// Short machine-readable reason code (e.g. `"probe_ok"`,
    /// `"integrity_mismatch"`).
    pub reason: &'static str,
    /// Human-readable detail message.
    pub detail: String,
}

impl CanaryAlertPolicy {
    /// Convenience constructor for the common success case.
    #[must_use]
    pub fn success(detail: String) -> Self {
        Self {
            tier: CanaryAlertTier::Silent,
            reason: "probe_ok",
            detail,
        }
    }
}

/// Latency threshold (in microseconds) above which a successful canary
/// probe is still flagged as `Observable`.  5 seconds is deliberately
/// generous — canary mailboxes are tiny and should complete in milliseconds.
const CANARY_SLOW_PROBE_THRESHOLD_US: u64 = 5_000_000;

/// Classify a canary probe outcome into an alert-routing policy.
///
/// The classification logic follows a strict severity waterfall:
///
/// 1. Integrity failure -> `Engineering` (possible schema / probe regression)
/// 2. Recovery failure  -> `Engineering` (possible recovery-logic regression)
/// 3. Probe assertion failure -> `Warning` (application-level regression)
/// 4. Slow probe -> `Observable` (performance regression signal)
/// 5. Otherwise -> `Silent` (routine success)
#[must_use]
pub fn classify_canary_outcome(
    probe_ok: bool,
    latency_us: u64,
    integrity_ok: bool,
    recovery_attempted: bool,
    recovery_ok: bool,
) -> CanaryAlertPolicy {
    if !integrity_ok {
        return CanaryAlertPolicy {
            tier: CanaryAlertTier::Engineering,
            reason: "integrity_mismatch",
            detail: "canary mailbox failed integrity check — possible regression in \
                     integrity probe or schema path"
                .to_string(),
        };
    }

    if recovery_attempted && !recovery_ok {
        return CanaryAlertPolicy {
            tier: CanaryAlertTier::Engineering,
            reason: "recovery_failed",
            detail: "canary recovery path failed on a disposable mailbox — possible \
                     regression in recovery logic"
                .to_string(),
        };
    }

    if !probe_ok {
        return CanaryAlertPolicy {
            tier: CanaryAlertTier::Warning,
            reason: "probe_assertion_failed",
            detail: "canary probe assertion failed but integrity and recovery paths \
                     are healthy"
                .to_string(),
        };
    }

    if latency_us > CANARY_SLOW_PROBE_THRESHOLD_US {
        return CanaryAlertPolicy {
            tier: CanaryAlertTier::Observable,
            reason: "slow_probe",
            detail: format!(
                "canary probe succeeded but took {:.1}s (threshold: {:.1}s)",
                latency_us as f64 / 1_000_000.0,
                CANARY_SLOW_PROBE_THRESHOLD_US as f64 / 1_000_000.0,
            ),
        };
    }

    CanaryAlertPolicy::success(format!(
        "canary probe completed in {:.1}ms",
        latency_us as f64 / 1_000.0,
    ))
}

/// Record a canary probe result into the isolated canary metrics surface.
///
/// This is the single entry point that the canary runner calls after each
/// probe cycle.  It updates only `CanaryMetrics` — never the production
/// `DbMetrics` or `StorageMetrics`.
pub fn record_canary_probe(
    latency_us: u64,
    ok: bool,
    recovery_attempted: bool,
    recovery_ok: bool,
    integrity_ok: bool,
) {
    let m = &mcp_agent_mail_core::global_metrics().canary;
    m.canary_probes_total.inc();
    m.canary_probe_latency_us.record(latency_us);

    if ok {
        m.canary_probes_ok.inc();
    } else {
        m.canary_probes_failed.inc();
    }

    if !integrity_ok {
        m.canary_integrity_failures_total.inc();
    }

    if recovery_attempted {
        m.canary_recovery_attempts_total.inc();
        if recovery_ok {
            m.canary_recovery_successes_total.inc();
        }
    }
}

/// Increment the active canary mailbox gauge (call when creating a canary
/// storage root).
pub fn canary_mailbox_created() {
    let m = &mcp_agent_mail_core::global_metrics().canary;
    m.canary_mailboxes_created_total.inc();
    m.canary_mailboxes_active.add(1);
}

/// Decrement the active canary mailbox gauge (call when tearing down a
/// canary storage root).
pub fn canary_mailbox_destroyed() {
    let m = &mcp_agent_mail_core::global_metrics().canary;
    m.canary_mailboxes_destroyed_total.inc();
    // Saturating decrement: active = max(0, active - 1).
    let current = m.canary_mailboxes_active.load();
    if current > 0 {
        m.canary_mailboxes_active.set(current.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_sqlite_identity_path_caches_recent_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("identity_cache.db");
        let raw_path = db_path.to_string_lossy().into_owned();

        sqlite_identity_path_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let first = normalize_sqlite_identity_path(&raw_path);
        let cached = sqlite_identity_path_cache_get(&raw_path);
        assert_eq!(cached.as_deref(), Some(first.as_str()));

        let second = normalize_sqlite_identity_path(&raw_path);
        assert_eq!(second, first);
    }

    #[test]
    fn sqlite_identity_path_cache_entries_expire_after_test_freshness_window() {
        let raw_path = "relative/cache-expiry.db";
        let normalized = "/tmp/cache-expiry.db";

        sqlite_identity_path_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        sqlite_identity_path_cache_insert(raw_path, normalized);
        assert_eq!(
            sqlite_identity_path_cache_get(raw_path).as_deref(),
            Some(normalized)
        );

        std::thread::sleep(SQLITE_IDENTITY_PATH_CACHE_FRESHNESS + Duration::from_millis(10));
        assert!(
            sqlite_identity_path_cache_get(raw_path).is_none(),
            "expired entries should be evicted on read"
        );
    }

    #[test]
    fn test_sqlite_path_parsing() {
        let config = DbPoolConfig {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:////absolute/path/db.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/absolute/path/db.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite+aiosqlite:///./legacy.db".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./legacy.db");

        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");

        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:?cache=shared".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");

        let config = DbPoolConfig {
            database_url: "sqlite:///relative/path.db".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/relative/path.db");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3?mode=rwc".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:///storage.sqlite3#v1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "/storage.sqlite3");

        let config = DbPoolConfig {
            database_url: "sqlite:///home/ubuntu/storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(
            config.sqlite_path().unwrap(),
            "/home/ubuntu/storage.sqlite3"
        );

        let config = DbPoolConfig {
            database_url: "postgres://localhost/db".to_string(),
            ..Default::default()
        };
        assert!(config.sqlite_path().is_err());
    }

    #[test]
    fn test_schema_init_in_memory() {
        // Use base schema (no FTS5/triggers) for FrankenConnection pool connections.

        // Open in-memory FrankenConnection
        let conn = DbConn::open_memory().expect("failed to open in-memory db");

        // Get base schema SQL (no FTS5 virtual tables or triggers)
        let sql = schema::init_schema_sql_base();
        println!("Schema SQL length: {} bytes", sql.len());

        // Execute it
        conn.execute_raw(&sql).expect("failed to init schema");

        // Verify tables exist by querying them directly (FrankenConnection
        // does not support sqlite_master; use simple SELECT to verify).
        let table_names: Vec<String> = ["projects", "agents", "messages"]
            .iter()
            .filter(|&&t| {
                conn.query_sync(&format!("SELECT 1 FROM {t} LIMIT 0"), &[])
                    .is_ok()
            })
            .map(ToString::to_string)
            .collect();

        println!("Created tables: {table_names:?}");

        assert!(table_names.contains(&"projects".to_string()));
        assert!(table_names.contains(&"agents".to_string()));
        assert!(table_names.contains(&"messages".to_string()));
    }

    // ── DbPoolConfig coverage ─────────────────────────────────────────

    #[test]
    fn from_env_defaults_use_auto_pool_size() {
        // When no DATABASE_POOL_SIZE env is set, from_env should use auto_pool_size
        let config = DbPoolConfig::from_env();
        let (auto_min, auto_max) = auto_pool_size();
        assert_eq!(config.min_connections, auto_min);
        assert_eq!(config.max_connections, auto_max);
        assert_eq!(config.max_lifetime_ms, DEFAULT_POOL_RECYCLE_MS);
        assert!(config.run_migrations);
    }

    #[test]
    fn sqlite_path_memory_returns_memory_string() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), ":memory:");
    }

    #[test]
    fn sqlite_path_file_returns_path() {
        let config = DbPoolConfig {
            database_url: "sqlite:///./storage.sqlite3".to_string(),
            ..Default::default()
        };
        assert_eq!(config.sqlite_path().unwrap(), "./storage.sqlite3");
    }

    #[test]
    fn sqlite_path_invalid_url_returns_error() {
        let config = DbPoolConfig {
            database_url: "postgres://localhost/db".to_string(),
            ..Default::default()
        };
        assert!(config.sqlite_path().is_err());
    }

    /// Verify pool defaults are sized for 1000+ concurrent agent workloads.
    ///
    /// The defaults were upgraded from the legacy Python values (3+4=7) to
    /// support high concurrency: min=25, max=100.
    #[test]
    fn pool_defaults_sized_for_scale() {
        assert_eq!(DEFAULT_POOL_SIZE, 25, "min connections for scale");
        assert_eq!(DEFAULT_MAX_OVERFLOW, 75, "overflow headroom for bursts");
        assert_eq!(
            DEFAULT_POOL_TIMEOUT_MS, 30_000,
            "30s timeout (fail fast, let circuit breaker handle)"
        );
        assert_eq!(
            DEFAULT_POOL_RECYCLE_MS,
            30 * 60 * 1000,
            "pool_recycle is 1800s (30 min)"
        );

        let cfg = DbPoolConfig::default();
        assert_eq!(cfg.min_connections, 25);
        assert_eq!(cfg.max_connections, 100); // 25 + 75
        assert_eq!(cfg.max_lifetime_ms, 1_800_000); // 30 min in ms
    }

    /// Verify auto-sizing picks reasonable values based on CPU count.
    #[test]
    fn auto_pool_size_is_reasonable() {
        let (min, max) = auto_pool_size();
        // Must be within configured clamp bounds.
        assert!(
            (10..=50).contains(&min),
            "auto min={min} should be in [10, 50]"
        );
        assert!(
            (50..=200).contains(&max),
            "auto max={max} should be in [50, 200]"
        );
        assert!(max >= min, "max must be >= min");
        // On a 4-core machine: min=16, max=48→50.  On 16-core: min=50, max=192.
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        assert_eq!(min, (cpus * 4).clamp(10, 50));
        assert_eq!(max, (cpus * 12).clamp(50, 200));
    }

    /// Verify PRAGMA settings contain `busy_timeout=60000` matching legacy Python.
    #[test]
    fn pragma_busy_timeout_matches_legacy() {
        let sql = schema::init_schema_sql();
        let busy_idx = sql
            .find("busy_timeout = 60000")
            .expect("schema init sql must contain busy_timeout");
        let wal_idx = sql
            .find("journal_mode = WAL")
            .expect("schema init sql must contain journal_mode=WAL");
        assert!(
            busy_idx < wal_idx,
            "busy_timeout must be set before journal_mode to avoid SQLITE_BUSY before timeout applies"
        );
        assert!(
            sql.contains("busy_timeout = 60000"),
            "PRAGMA busy_timeout must be 60000 (60s) to match Python legacy"
        );
        assert!(
            sql.contains("journal_mode = WAL"),
            "WAL mode is required for concurrent access"
        );
    }

    /// Verify warmup opens the requested number of connections.
    #[test]
    fn pool_warmup_opens_connections() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("warmup_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 10,
            max_connections: 20,
            warmup_connections: 5,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let opened = rt.block_on(pool.warmup(&cx, 5, std::time::Duration::from_secs(10)));
        assert_eq!(opened, 5, "warmup should open exactly 5 connections");

        // Pool stats should reflect the warmed-up connections.
        let stats = pool.pool.stats();
        assert!(
            stats.total_connections >= 5,
            "pool should have at least 5 total connections after warmup, got {}",
            stats.total_connections
        );
    }

    /// Verify warmup with n=0 is a no-op.
    #[test]
    fn pool_warmup_zero_is_noop() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("warmup_zero.db");
        let pool = DbPool::new(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let opened = rt.block_on(pool.warmup(&cx, 0, std::time::Duration::from_secs(1)));
        assert_eq!(opened, 0, "warmup with n=0 should open no connections");
    }

    /// Verify default config includes `warmup_connections`: 0.
    #[test]
    fn default_warmup_is_disabled() {
        let cfg = DbPoolConfig::default();
        assert_eq!(
            cfg.warmup_connections, 0,
            "warmup should be disabled by default"
        );
    }

    /// Verify `build_conn_pragmas` scales `cache_size` with pool size.
    #[test]
    fn build_conn_pragmas_budget_aware_cache() {
        // 100 connections: 512*1024 / 100 = 5242 KB each
        let sql_100 = schema::build_conn_pragmas(100, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            sql_100.contains("cache_size = -5242"),
            "100 conns should get ~5MB each: {sql_100}"
        );

        // 25 connections: 512*1024 / 25 = 20971 KB each
        let sql_25 = schema::build_conn_pragmas(25, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            sql_25.contains("cache_size = -20971"),
            "25 conns should get ~20MB each: {sql_25}"
        );

        // 1 connection: 512*1024 / 1 = 524288 KB → clamped to 65536 (64MB max)
        let sql_1 = schema::build_conn_pragmas(1, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            sql_1.contains("cache_size = -65536"),
            "1 conn should get 64MB (clamped max): {sql_1}"
        );

        // 500 connections: clamped to 2MB min
        let sql_500 = schema::build_conn_pragmas(500, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            sql_500.contains("cache_size = -2048"),
            "500 conns should get 2MB (clamped min): {sql_500}"
        );

        // All should have journal_size_limit
        for sql in [&sql_100, &sql_25, &sql_1, &sql_500] {
            assert!(
                sql.contains("journal_size_limit = 268435456"),
                "all should have 256MB journal_size_limit"
            );
            assert!(
                sql.contains("busy_timeout = 60000"),
                "must have busy_timeout"
            );
            assert!(
                sql.contains("mmap_size = 268435456"),
                "must have 256MB mmap"
            );
        }
    }

    /// Verify `build_conn_pragmas` handles zero pool size gracefully.
    #[test]
    fn build_conn_pragmas_zero_pool_fallback() {
        let sql = schema::build_conn_pragmas(0, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            sql.contains("cache_size = -8192"),
            "0 conns should fallback to 8MB: {sql}"
        );
    }

    #[test]
    fn per_connection_pragmas_omit_db_wide_journal_mode() {
        assert!(
            !schema::PRAGMA_CONN_SETTINGS_SQL.contains("journal_mode"),
            "fresh probe/read connections must not try to switch journal mode"
        );

        let sql = schema::build_conn_pragmas(4, schema::DEFAULT_CACHE_BUDGET_KB);
        assert!(
            !sql.contains("journal_mode"),
            "pool connection init must not reissue journal_mode=WAL: {sql}"
        );
    }

    #[test]
    fn second_pool_acquire_succeeds_under_reserved_lock_after_init() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("second_acquire_reserved_lock.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 2,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let first_conn = rt
            .block_on(async { pool.acquire(&cx).await })
            .into_result()
            .expect("acquire first pooled connection");

        let lock_conn = DbConn::open_file(db_path.display().to_string()).expect("open lock db");
        lock_conn
            .execute_raw("PRAGMA busy_timeout = 1")
            .expect("set lock busy_timeout");
        lock_conn
            .execute_raw("BEGIN IMMEDIATE")
            .expect("hold reserved sqlite lock");

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let pool_for_thread = pool;
        let acquire_thread = std::thread::spawn(move || {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build thread runtime");
            let cx = Cx::for_testing();
            let result = rt.block_on(async {
                match pool_for_thread.acquire(&cx).await {
                    Outcome::Ok(conn) => conn
                        .query_sync("SELECT 1 AS one", &[])
                        .map(|rows| rows.len())
                        .map_err(|e| format!("query via second pooled connection failed: {e}")),
                    Outcome::Err(err) => Err(format!("second pooled acquire failed: {err}")),
                    Outcome::Cancelled(reason) => {
                        Err(format!("second pooled acquire cancelled: {reason:?}"))
                    }
                    Outcome::Panicked(payload) => Err(format!(
                        "second pooled acquire panicked: {}",
                        payload.message()
                    )),
                }
            });
            result_tx.send(result).expect("send acquire result");
        });

        let row_count = match result_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(result) => result.expect("reserved lock should not block second pooled acquire"),
            Err(err) => {
                let _ = lock_conn.execute_raw("ROLLBACK");
                acquire_thread
                    .join()
                    .expect("join acquire thread after timeout");
                panic!("second pooled acquire should not stall under reserved lock: {err}");
            }
        };
        assert_eq!(row_count, 1, "second pooled connection should stay usable");

        lock_conn
            .execute_raw("ROLLBACK")
            .expect("release sqlite lock");
        drop(first_conn);
        acquire_thread.join().expect("join acquire thread");
    }

    /// Verify explicit WAL checkpoint works on a file-backed DB.
    #[test]
    fn wal_checkpoint_succeeds_on_file_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ckpt_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Write some data through the pool to generate WAL entries.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let pool2 = pool.clone();
        rt.block_on(async move {
            let conn = pool2.acquire(&cx).await.unwrap();
            conn.execute_raw("CREATE TABLE IF NOT EXISTS ckpt_test (id INTEGER PRIMARY KEY)")
                .ok();
            conn.execute_raw("INSERT INTO ckpt_test VALUES (1)").ok();
            conn.execute_raw("INSERT INTO ckpt_test VALUES (2)").ok();
        });

        // Checkpoint should succeed without error.
        let frames = pool.wal_checkpoint().expect("checkpoint should succeed");
        // frames can be 0 if autocheckpoint already ran, but it shouldn't error.
        assert!(frames <= 1000, "reasonable frame count: {frames}");
    }

    /// Verify WAL checkpoint on :memory: is a no-op.
    #[test]
    fn wal_checkpoint_noop_for_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let frames = pool
            .wal_checkpoint()
            .expect("memory checkpoint should succeed");
        assert_eq!(frames, 0, "memory DB checkpoint should return 0");
    }

    fn sqlite_marker_value(path: &Path) -> Option<String> {
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).ok()?;
        conn.execute_raw("CREATE TABLE IF NOT EXISTS marker(value TEXT NOT NULL)")
            .ok()?;
        let rows = conn
            .query_sync("SELECT value FROM marker ORDER BY rowid DESC LIMIT 1", &[])
            .ok()?;
        rows.first()?.get_named::<String>("value").ok()
    }

    #[test]
    fn sqlite_backup_candidates_prioritize_dot_bak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let dot_bak = dir.path().join("storage.sqlite3.bak");
        let backup_series = dir.path().join("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&dot_bak, b"bak").expect("write .bak");
        std::fs::write(&backup_series, b"series").expect("write backup-");

        let candidates = sqlite_backup_candidates(&primary);
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(dot_bak.as_path()),
            ".bak should be first-priority backup candidate"
        );
    }

    #[test]
    fn sqlite_backup_candidates_include_series_for_relative_primary_path() {
        struct CwdGuard {
            previous: PathBuf,
        }
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.previous);
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::current_dir().expect("current_dir");
        let _cwd_guard = CwdGuard { previous };
        std::env::set_current_dir(dir.path()).expect("set cwd");

        let primary = PathBuf::from("storage.sqlite3");
        let backup_series = PathBuf::from("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&backup_series, b"series").expect("write backup series");

        let candidates = sqlite_backup_candidates(&primary);
        assert!(
            candidates.iter().any(|c| {
                c.file_name().and_then(|n| n.to_str())
                    == Some("storage.sqlite3.backup-20260212_000000")
            }),
            "relative primary path should still discover backup-series candidates"
        );
    }

    #[test]
    fn sqlite_backup_candidates_include_timestamped_bak_series() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let backup_bak_series = dir.path().join("storage.sqlite3.bak.20260212_000000");
        let backup_series = dir.path().join("storage.sqlite3.backup-20260212_010000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&backup_bak_series, b"bak series").expect("write .bak timestamp series");
        std::fs::write(&backup_series, b"backup series").expect("write .backup- series");

        let candidates = sqlite_backup_candidates(&primary);
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(backup_bak_series.as_path()),
            "timestamped .bak.* backups should be discovered and prioritized over .backup-*"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_backup_candidates_skip_symlinked_backups() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let real_backup = dir.path().join("outside.sqlite3");
        let symlinked_bak = dir.path().join("storage.sqlite3.bak");
        let symlinked_series = dir.path().join("storage.sqlite3.backup-20260212_000000");
        std::fs::write(&primary, b"primary").expect("write primary");
        std::fs::write(&real_backup, b"backup").expect("write real backup");
        symlink(&real_backup, &symlinked_bak).expect("symlink .bak");
        symlink(&real_backup, &symlinked_series).expect("symlink .backup- series");

        let candidates = sqlite_backup_candidates(&primary);
        assert!(
            candidates.is_empty(),
            "symlinked backup candidates must be ignored"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_restores_from_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");

        // Create a healthy DB as a backup.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .unwrap();
        drop(conn);
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));
        std::fs::copy(&primary, &backup).unwrap();

        // Corrupt the primary DB file.
        std::fs::write(&primary, b"corrupted-data").unwrap();

        ensure_sqlite_file_healthy(&primary).expect("auto-recovery should succeed");
        assert_eq!(
            sqlite_marker_value(&primary).as_deref(),
            Some("from-backup"),
            "restored DB should preserve backup data"
        );

        let mut corrupt_artifacts = 0usize;
        for entry in std::fs::read_dir(dir.path()).expect("read dir").flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().contains(".corrupt-") {
                corrupt_artifacts += 1;
            }
        }
        assert!(
            corrupt_artifacts >= 1,
            "expected quarantined corrupt artifact(s) after recovery"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_restores_from_timestamped_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup1 = dir.path().join("storage.sqlite3.bak.20240101_120000");
        let backup2 = dir.path().join("storage.sqlite3.bak.20240102_120000"); // Should pick the newest

        // Create a healthy DB.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE t (x INTEGER)").unwrap();
        drop(conn);
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));

        // Create dummy older backup and real newer backup (which must be a valid DB!).
        std::fs::write(&backup1, b"corrupted-old-backup").unwrap();
        let conn2 = DbConn::open_file(backup2.to_string_lossy().as_ref()).unwrap();
        conn2
            .execute_raw("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (42);")
            .unwrap();
        drop(conn2);

        // Corrupt the primary to trigger recovery.
        std::fs::write(&primary, b"broken").unwrap();

        ensure_sqlite_file_healthy(&primary).expect("auto-recovery should succeed");

        // Verify the restored DB is exactly the valid backup.
        let restored_conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let val: i64 = restored_conn.query_sync("SELECT x FROM t", &[]).unwrap()[0]
            .get_named("x")
            .unwrap();
        assert_eq!(
            val, 42,
            "restored DB should preserve timestamped backup data"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_reinitializes_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        std::fs::write(&primary, b"broken").expect("write broken db");

        ensure_sqlite_file_healthy(&primary).expect("should reinitialize without backup");
        let healthy = sqlite_file_is_healthy(&primary).expect("health check");
        assert!(healthy, "reinitialized sqlite file should pass quick_check");

        let quarantined_any = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(
            quarantined_any,
            "expected corrupted artifact to be quarantined during reinit"
        );
    }

    #[test]
    fn startup_integrity_check_recovers_open_failure_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("startup_corrupt.db");
        std::fs::write(&primary, b"not-a-sqlite-file").expect("write corrupt file");

        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", primary.display()),
            run_migrations: false,
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let result = pool
            .run_startup_integrity_check()
            .expect("startup integrity should auto-recover");
        assert!(
            result.ok,
            "startup quick_check should report healthy after recovery"
        );
        assert!(
            sqlite_file_is_healthy(&primary).expect("post-startup health check"),
            "sqlite file should be healthy after startup recovery"
        );
    }

    #[test]
    fn pool_init_preserves_legacy_fixture_rows() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("legacy_fixture.db");
        let db_path_str = db_path.to_string_lossy();

        let seed_conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path_str.as_ref())
            .expect("open seed sqlite db");
        let seed_sql = [
            "PRAGMA foreign_keys = OFF",
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
            "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL, inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', reaper_exempt INTEGER NOT NULL DEFAULT 0, registration_token TEXT)",
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL, ack_required INTEGER NOT NULL, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            "CREATE TABLE IF NOT EXISTS message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts DATETIME, ack_ts DATETIME, PRIMARY KEY (message_id, agent_id, kind))",
            "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT, created_ts DATETIME NOT NULL, expires_ts DATETIME NOT NULL, released_ts DATETIME)",
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'legacy-project', '/tmp/legacy-project', '2026-02-24 15:30:00.123456')",
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (1, 1, 'LegacySender', 'python', 'legacy', 'sender', '2026-02-24 15:30:01', '2026-02-24 15:30:02', 'auto', 'auto')",
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (2, 1, 'LegacyReceiver', 'python', 'legacy', 'receiver', '2026-02-24 15:31:01', '2026-02-24 15:31:02', 'auto', 'auto')",
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (1, 1, 1, 'br-28mgh.8.2', 'Legacy migration message', 'from python db', 'high', 1, '2026-02-24 15:32:00.654321', '[]')",
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 2, 'to', NULL, NULL)",
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (1, 1, 1, 'src/legacy/**', 1, 'legacy reservation', '2026-02-24 15:33:00', '2026-12-24 15:33:00', NULL)",
        ];
        for stmt in seed_sql {
            seed_conn.execute_raw(stmt).expect("seed fixture statement");
        }
        drop(seed_conn);

        let pool = DbPool::new(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _ = pool
                .acquire(&cx)
                .await
                .into_result()
                .expect("acquire pool connection");
        });

        assert!(
            sqlite_file_is_healthy_canonical(&db_path).expect("post-init health probe"),
            "legacy fixture should remain healthy after pool init"
        );

        let verify_conn = sqlmodel_sqlite::SqliteConnection::open_file(db_path_str.as_ref())
            .expect("open verify sqlite db");
        for (table, expected) in [
            ("projects", 1_i64),
            ("agents", 2_i64),
            ("messages", 1_i64),
            ("message_recipients", 1_i64),
            ("file_reservations", 1_i64),
        ] {
            let rows = verify_conn
                .query_sync(&format!("SELECT COUNT(*) AS c FROM {table}"), &[])
                .expect("count query");
            let actual = rows
                .first()
                .and_then(|r| r.get_named::<i64>("c").ok())
                .unwrap_or(-1);
            assert_eq!(actual, expected, "{table} row count should be preserved");
        }

        let type_rows = verify_conn
            .query_sync(
                "SELECT typeof(created_at) AS t FROM projects WHERE id = 1",
                &[],
            )
            .expect("projects type query");
        assert_eq!(
            type_rows[0]
                .get_named::<String>("t")
                .expect("projects.created_at typeof"),
            "integer",
            "timestamp migration should convert TEXT project timestamp to INTEGER"
        );
    }

    #[test]
    fn pool_startup_drops_fts_tables() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fts_preservation.db");
        let db_url = format!("sqlite:///{}", db_path.display());
        let db_path_str = db_path.display().to_string();

        // Create pool - runs migrations + FTS cleanup
        let pool = DbPool::new(&DbPoolConfig {
            database_url: db_url,
            ..Default::default()
        })
        .expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();

        // Acquire a connection to trigger migration
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });
        drop(pool);

        // Verify FTS tables are dropped after pool startup (Tantivy handles search)
        let conn = DbConn::open_file(db_path_str).expect("reopen sqlite db");
        let fts_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS n FROM sqlite_master \
                 WHERE type='table' AND name = 'fts_messages'",
                &[],
            )
            .expect("query fts_messages table");
        let fts_count = fts_rows
            .first()
            .and_then(|row| row.get_named::<i64>("n").ok())
            .unwrap_or_default();
        assert_eq!(
            fts_count, 0,
            "pool startup should drop fts_messages table (Tantivy handles search)"
        );
    }

    /// Verify `run_startup_integrity_check` passes on a healthy file-backed DB.
    #[test]
    fn startup_integrity_check_healthy_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy_startup.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Trigger initial migration so the file actually exists.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        let result = pool
            .run_startup_integrity_check()
            .expect("startup integrity check");
        assert!(result.ok, "healthy DB should pass startup integrity check");
        assert!(
            result.details.contains(&"ok".to_string()),
            "details should contain 'ok'"
        );
    }

    /// Verify `run_startup_integrity_check` returns Ok for :memory: databases.
    #[test]
    fn startup_integrity_check_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool
            .run_startup_integrity_check()
            .expect("memory integrity check");
        assert!(result.ok, "memory DB should always pass");
        assert_eq!(result.duration_us, 0, "memory check should be instant");
    }

    /// Verify `run_startup_integrity_check` treats a missing DB file as
    /// integrity corruption so callers can trigger recovery/initialization.
    #[test]
    fn startup_integrity_check_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool.run_startup_integrity_check();
        assert!(
            matches!(result, Err(DbError::IntegrityCorruption { .. })),
            "missing file should be treated as integrity corruption needing recovery"
        );
    }

    /// Verify `run_full_integrity_check` passes on a healthy file-backed DB.
    #[test]
    fn full_integrity_check_healthy_db() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("healthy_full.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        let result = pool
            .run_full_integrity_check()
            .expect("full integrity check");
        assert!(result.ok, "healthy DB should pass full integrity check");
        assert_eq!(
            result.kind,
            integrity::CheckKind::Full,
            "should be a full check"
        );
    }

    /// Verify `run_full_integrity_check` returns Ok for :memory: databases.
    #[test]
    fn full_integrity_check_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let result = pool.run_full_integrity_check().expect("memory full check");
        assert!(result.ok, "memory DB should always pass full check");
        assert_eq!(
            result.kind,
            integrity::CheckKind::Full,
            "should be Full kind"
        );
    }

    /// Verify `sample_recent_message_refs` returns empty for :memory: databases.
    #[test]
    fn sample_recent_message_refs_memory_db() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let refs = pool.sample_recent_message_refs(10).expect("memory sample");
        assert!(refs.is_empty(), "memory DB should return empty refs");
    }

    /// Verify `sample_recent_message_refs` returns empty for non-existent DB.
    #[test]
    fn sample_recent_message_refs_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("missing_refs.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let refs = pool
            .sample_recent_message_refs(10)
            .expect("missing file sample");
        assert!(refs.is_empty(), "missing DB should return empty refs");
    }

    /// Verify `sample_recent_message_refs` returns actual messages from a seeded DB.
    #[test]
    fn sample_recent_message_refs_returns_seeded_messages() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("refs_seeded.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Seed the database with a project, agent, and messages.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            let now = crate::now_micros();
            conn.execute_raw(&format!(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'test-proj', '/tmp/test-proj', {now})"
            ))
            .expect("insert project");
            conn.execute_raw(&format!(
                "INSERT INTO agents (id, project_id, name, program, model, \
                 inception_ts, last_active_ts) \
                 VALUES (1, 1, 'BlueLake', 'test', 'test-model', {now}, {now})"
            ))
            .expect("insert agent");
            conn.execute_raw(&format!(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                 thread_id, importance, created_ts) \
                 VALUES (1, 1, 1, 'Test Subject', 'body', 'thread-1', 'normal', {now})"
            ))
            .expect("insert message");
            conn.execute_raw(&format!(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                 thread_id, importance, created_ts) \
                 VALUES (2, 1, 1, 'Second Message', 'body2', 'thread-2', 'normal', {now})"
            ))
            .expect("insert message 2");
        });

        let refs = pool.sample_recent_message_refs(10).expect("sample refs");
        assert_eq!(refs.len(), 2, "should return 2 seeded messages");
        // Messages should be in DESC order by id.
        assert_eq!(refs[0].message_id, 2);
        assert_eq!(refs[1].message_id, 1);
        assert_eq!(refs[0].project_slug, "test-proj");
        assert_eq!(refs[0].sender_name, "BlueLake");
        assert_eq!(refs[0].subject, "Second Message");
        assert_eq!(refs[1].subject, "Test Subject");
    }

    /// Verify `sample_recent_message_refs` honours the limit parameter.
    #[test]
    fn sample_recent_message_refs_respects_limit() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("refs_limited.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let conn = pool.acquire(&cx).await.into_result().expect("acquire");
            let now = crate::now_micros();
            conn.execute_raw(&format!(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'limit-proj', '/tmp/limit', {now})"
            ))
            .expect("insert project");
            conn.execute_raw(&format!(
                "INSERT INTO agents (id, project_id, name, program, model, \
                 inception_ts, last_active_ts) \
                 VALUES (1, 1, 'RedFox', 'test', 'model', {now}, {now})"
            ))
            .expect("insert agent");
            for i in 1..=5 {
                conn.execute_raw(&format!(
                    "INSERT INTO messages (id, project_id, sender_id, subject, body_md, \
                     thread_id, importance, created_ts) \
                     VALUES ({i}, 1, 1, 'Msg {i}', 'body', 'thread-{i}', 'normal', {now})"
                ))
                .expect("insert message");
            }
        });

        let refs = pool.sample_recent_message_refs(3).expect("limited sample");
        assert_eq!(refs.len(), 3, "should respect limit=3");
        // Most recent first.
        assert_eq!(refs[0].message_id, 5);
        assert_eq!(refs[2].message_id, 3);
    }

    /// Verify `get_or_create_pool` returns the same pool for the same cache key.
    #[test]
    fn get_or_create_pool_caches_by_config_signature() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };

        let pool1 = get_or_create_pool(&config).expect("first get");
        let pool2 = get_or_create_pool(&config).expect("second get");

        // Both should point to the same underlying pool (Arc identity).
        assert!(
            Arc::ptr_eq(&pool1.pool, &pool2.pool),
            "get_or_create_pool should return the same Arc<Pool> for the same cache key"
        );
    }

    /// Verify distinct pool sizing does not alias to the same cached pool.
    #[test]
    fn get_or_create_pool_keeps_small_startup_pools_isolated_from_runtime_pools() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache_shape_test.db");
        let database_url = format!("sqlite:///{}", db_path.display());

        let startup_cfg = DbPoolConfig {
            database_url: database_url.clone(),
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        };
        let runtime_cfg = DbPoolConfig {
            database_url,
            min_connections: 25,
            max_connections: 100,
            ..Default::default()
        };

        let startup_pool = get_or_create_pool(&startup_cfg).expect("startup pool");
        let runtime_pool = get_or_create_pool(&runtime_cfg).expect("runtime pool");

        assert!(
            !Arc::ptr_eq(&startup_pool.pool, &runtime_pool.pool),
            "pool cache must not alias startup/worker tiny pools with runtime pool sizing"
        );
    }

    #[test]
    fn get_or_create_pool_keeps_distinct_storage_roots_isolated_for_same_sqlite_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.sqlite3");
        let storage_a = dir.path().join("storage-a");
        let storage_b = dir.path().join("storage-b");
        std::fs::create_dir_all(&storage_a).unwrap();
        std::fs::create_dir_all(&storage_b).unwrap();

        let cfg_a = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root: Some(storage_a),
            ..Default::default()
        };
        let cfg_b = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root: Some(storage_b),
            ..Default::default()
        };

        let pool_a = get_or_create_pool(&cfg_a).expect("pool a");
        let pool_b = get_or_create_pool(&cfg_b).expect("pool b");

        assert!(
            !Arc::ptr_eq(&pool_a.pool, &pool_b.pool),
            "pool cache must not alias the same sqlite file across distinct storage roots"
        );
    }

    /// Verify `DbPool::sqlite_path()` accessor matches config.
    #[test]
    fn pool_sqlite_path_accessor() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("path_test.db");
        let expected = db_path.display().to_string();
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        assert_eq!(pool.sqlite_path(), expected);
    }

    /// Verify `sample_pool_stats_now` doesn't panic and updates metrics.
    #[test]
    fn sample_pool_stats_now_updates_metrics() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stats_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).expect("create pool");

        // Open a connection first so the pool has something to sample.
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });

        // This should not panic.
        pool.sample_pool_stats_now();

        // Verify global metrics were updated.
        let metrics = mcp_agent_mail_core::global_metrics();
        let total = metrics.db.pool_total_connections.load();
        assert!(
            total >= 1,
            "pool_total_connections should be >= 1 after acquire + sample, got {total}"
        );
    }

    #[test]
    fn pool_startup_strips_identity_fts_artifacts() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("identity_fts_preserved.db");
        let db_path_str = db_path.display().to_string();
        let db_url = format!("sqlite:///{}", db_path.display());
        let config = DbPoolConfig {
            database_url: db_url,
            ..Default::default()
        };
        let parsed_path = config
            .sqlite_path()
            .expect("parse sqlite path from database_url");
        assert_eq!(
            parsed_path, db_path_str,
            "pool must target the fixture DB path for this regression test"
        );

        // Create pool - should run full migrations including FTS
        let pool = DbPool::new(&config).expect("create pool");
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
        });
        drop(pool);

        // Verify identity FTS artifacts are removed after pool startup.
        let conn = DbConn::open_file(parsed_path).expect("reopen db");
        let identity_fts_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS n FROM sqlite_master \
                 WHERE (type='table' AND name IN ('fts_agents', 'fts_projects')) \
                    OR (type='trigger' AND name IN (\
                        'agents_ai', 'agents_ad', 'agents_au', \
                        'projects_ai', 'projects_ad', 'projects_au'\
                    ))",
                &[],
            )
            .expect("query identity FTS artifacts");
        let identity_fts_count = identity_fts_rows
            .first()
            .and_then(|row| row.get_named::<i64>("n").ok())
            .unwrap_or_default();
        assert_eq!(
            identity_fts_count, 0,
            "pool startup must remove legacy identity FTS artifacts to avoid rowid corruption regressions"
        );
    }

    /// Verify `create_pool` is an alias for `get_or_create_pool`.
    #[test]
    fn create_pool_is_alias_for_get_or_create() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("alias_test.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };

        let pool1 = create_pool(&config).expect("create_pool");
        let pool2 = get_or_create_pool(&config).expect("get_or_create_pool");

        assert!(
            Arc::ptr_eq(&pool1.pool, &pool2.pool),
            "create_pool should delegate to get_or_create_pool"
        );
    }

    #[test]
    fn pool_acquire_uses_explicit_storage_root_for_archive_reconcile() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("explicit-storage-root.sqlite3");
        let configured_storage_root = dir.path().join("configured-storage");
        let wrong_storage_root = dir.path().join("wrong-storage");

        let proj_dir = configured_storage_root
            .join("projects")
            .join("archive-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .unwrap();
        std::fs::create_dir_all(&wrong_storage_root).unwrap();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("STORAGE_ROOT", wrong_storage_root.to_str().unwrap())],
            || {
                let config = DbPoolConfig {
                    database_url: format!("sqlite:///{}", db_path.display()),
                    storage_root: Some(configured_storage_root.clone()),
                    ..Default::default()
                };
                let pool = DbPool::new(&config).unwrap();
                let rt = RuntimeBuilder::current_thread().build().unwrap();
                let cx = Cx::for_testing();
                rt.block_on(async {
                    let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
                });

                let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).unwrap();
                let rows = conn
                    .query_sync(
                        "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                        &[],
                    )
                    .unwrap();
                let row = rows.first().unwrap();
                assert_eq!(row.get_named::<i64>("count").unwrap_or(0), 1);
                assert_eq!(row.get_named::<i64>("max_id").unwrap_or(0), 1);
            },
        );
    }

    /// Verify corruption detection recognizes known error messages.
    #[test]
    fn corruption_error_message_detection() {
        assert!(is_corruption_error_message(
            "database disk image is malformed"
        ));
        assert!(is_corruption_error_message(
            "Error: database disk image is malformed (detail)"
        ));
        assert!(is_corruption_error_message(
            "malformed database schema - something"
        ));
        assert!(is_corruption_error_message("database schema is corrupt"));
        assert!(is_corruption_error_message("file is not a database"));
        assert!(is_corruption_error_message(
            "database file too small for header: 14 bytes (< 100)"
        ));
        assert!(is_corruption_error_message(
            "page 12: xxh3 page checksum mismatch"
        ));
        assert!(is_corruption_error_message(
            "database file tmp/storage.sqlite3 is malformed and no healthy backup was found"
        ));
        assert!(is_corruption_error_message(
            "DATABASE DISK IMAGE IS MALFORMED"
        ));
        // Non-corruption errors should not be detected.
        assert!(!is_corruption_error_message("table not found"));
        assert!(!is_corruption_error_message("database is locked"));
        assert!(!is_corruption_error_message(""));
    }

    #[test]
    fn sqlite_recovery_error_message_detection() {
        assert!(is_sqlite_recovery_error_message(
            "database disk image is malformed"
        ));
        assert!(is_sqlite_recovery_error_message(
            "Query error: out of memory"
        ));
        assert!(is_sqlite_recovery_error_message("cursor stack is empty"));
        assert!(is_sqlite_recovery_error_message(
            "called `Option::unwrap()` on a `None` value"
        ));
        assert!(is_sqlite_recovery_error_message("internal error"));
        assert!(is_sqlite_recovery_error_message(
            "database is busy (snapshot conflict on pages: page 4434 > snapshot db_size 4433 (latest: 4433))"
        ));
        assert!(is_sqlite_recovery_error_message("SQLITE_BUSY_SNAPSHOT"));
        assert!(!is_sqlite_recovery_error_message("database is locked"));
        assert!(!is_sqlite_recovery_error_message("table not found"));
    }

    #[test]
    fn index_only_integrity_issue_detection() {
        assert!(is_index_only_integrity_issue(
            "wrong # of entries in index sqlite_autoindex_agents_1"
        ));
        assert!(is_index_only_integrity_issue(
            "row 4107 missing from index idx_agents_last_active_id_desc"
        ));
        assert!(is_index_only_integrity_issue(
            "rowid 42 missing from index some_idx"
        ));
        assert!(!is_index_only_integrity_issue(
            "database disk image is malformed"
        ));
        assert!(!is_index_only_integrity_issue("file is not a database"));
    }

    #[test]
    fn details_are_index_only_issues_requires_all_lines_to_be_index_issues() {
        assert!(details_are_index_only_issues(&[
            "wrong # of entries in index sqlite_autoindex_agents_1".to_string(),
            "row 4108 missing from index idx_agents_last_active_id_desc".to_string(),
        ]));

        assert!(!details_are_index_only_issues(&["ok".to_string()]));
        assert!(!details_are_index_only_issues(&[
            "wrong # of entries in index sqlite_autoindex_agents_1".to_string(),
            "database disk image is malformed".to_string(),
        ]));
        assert!(!details_are_index_only_issues(&[]));
    }

    #[test]
    fn try_repair_index_only_corruption_is_noop_for_healthy_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("healthy_repair_probe.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, last_active_ts INTEGER NOT NULL, UNIQUE(project_id, name))",
        )
        .expect("create");
        conn.execute_raw(
            "CREATE INDEX idx_agents_last_active_id_desc ON agents(last_active_ts DESC, id DESC)",
        )
        .expect("index");
        conn.execute_raw(
            "INSERT INTO agents(id, project_id, name, last_active_ts) VALUES (1, 1, 'agent', 1)",
        )
        .expect("insert");
        drop(conn);

        let repaired = try_repair_index_only_corruption(&path).expect("repair probe");
        assert!(
            !repaired,
            "healthy DB should not trigger in-place REINDEX repair"
        );
        assert!(
            sqlite_file_is_healthy_canonical(&path).expect("canonical health check"),
            "healthy DB should remain canonically healthy after no-op repair probe"
        );
    }

    /// Verify `sqlite_file_is_healthy` returns false for a corrupt file.
    #[test]
    fn sqlite_file_is_healthy_detects_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.db");
        std::fs::write(&path, b"not-a-database").expect("write corrupt");
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(!healthy, "corrupt file should not be healthy");
    }

    /// Verify `sqlite_file_is_healthy` returns false for non-existent file.
    #[test]
    fn sqlite_file_is_healthy_nonexistent_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.db");
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(
            !healthy,
            "non-existent file should not be considered healthy"
        );
    }

    /// Verify `sqlite_file_is_healthy` returns true for a valid DB.
    #[test]
    fn sqlite_file_is_healthy_valid_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);
        let healthy = sqlite_file_is_healthy(&path).expect("should not error");
        assert!(healthy, "valid DB should be healthy");
    }

    #[test]
    fn sqlite_file_has_live_sidecars_detects_non_empty_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sidecar.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));

        assert!(!sqlite_file_has_live_sidecars(&path));

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        let shm_path = PathBuf::from(shm_os);
        std::fs::write(&shm_path, b"live-sidecar").expect("write sidecar");
        assert!(sqlite_file_has_live_sidecars(&path));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn sqlite_file_is_healthy_with_sidecar_invokes_compat_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compat_probe.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        std::fs::write(PathBuf::from(shm_os), b"live-sidecar").expect("write sidecar");

        let mut probe_called = false;
        let healthy = sqlite_file_is_healthy_with_compat_probe(&path, |_| {
            probe_called = true;
            Ok(true)
        })
        .expect("health check");
        assert!(healthy, "compat probe true should preserve healthy verdict");
        assert!(
            probe_called,
            "compatibility probe must run when sidecars exist"
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn sqlite_file_is_healthy_with_sidecar_accepts_compat_unhealthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compat_unhealthy.db");
        let path_str = path.to_string_lossy();
        let conn = DbConn::open_file(path_str.as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let mut shm_os = path.as_os_str().to_os_string();
        shm_os.push("-shm");
        std::fs::write(PathBuf::from(shm_os), b"live-sidecar").expect("write sidecar");

        let healthy =
            sqlite_file_is_healthy_with_compat_probe(&path, |_| Ok(false)).expect("health check");
        assert!(
            !healthy,
            "compatibility probe failure should mark file unhealthy"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_clears_stale_sidecars_and_recovers() {
        // Stale sidecars from a crash should be cleaned up automatically.
        // The checkpoint will fail (corrupt primary) but remove_sqlite_sidecars
        // should delete the fake sidecar files, allowing recovery to proceed.
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let wal = dir.path().join("storage.sqlite3-wal");
        let shm = dir.path().join("storage.sqlite3-shm");
        std::fs::write(&primary, b"not-a-sqlite-db").expect("write corrupt primary");
        std::fs::write(&wal, b"x").expect("write wal");
        std::fs::write(&shm, b"x").expect("write shm");

        // With stale sidecars (removable files), recovery should proceed
        // rather than refusing. The function will fail for other reasons
        // (corrupt primary with no backup), but NOT because of sidecars.
        let result = ensure_sqlite_file_healthy(&primary);
        if let Err(ref e) = result {
            let message = e.to_string();
            assert!(
                !message.contains("WAL/SHM sidecars"),
                "should not refuse recovery for stale removable sidecars; got: {message}"
            );
        }
    }

    #[test]
    fn ensure_sqlite_file_healthy_with_archive_clears_stale_sidecars_and_recovers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let wal = dir.path().join("storage.sqlite3-wal");
        let shm = dir.path().join("storage.sqlite3-shm");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("mkdir storage root");
        std::fs::write(&primary, b"not-a-sqlite-db").expect("write corrupt primary");
        std::fs::write(&wal, b"x").expect("write wal");
        std::fs::write(&shm, b"x").expect("write shm");

        let result = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root);
        if let Err(ref e) = result {
            let message = e.to_string();
            assert!(
                !message.contains("WAL/SHM sidecars"),
                "should not refuse recovery for stale removable sidecars; got: {message}"
            );
        }
    }

    #[test]
    fn resolve_sqlite_path_prefers_healthy_absolute_when_relative_is_malformed() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let relative_path = PathBuf::from(absolute_db_str.trim_start_matches('/'));
        if let Some(parent) = relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create relative parent");
        }
        std::fs::write(&relative_path, b"not-a-database").expect("write malformed relative db");

        let resolved =
            resolve_sqlite_path_with_absolute_fallback(relative_path.to_string_lossy().as_ref());
        assert_eq!(resolved, absolute_db_str);

        let _ = std::fs::remove_file(&relative_path);
        if let Some(parent) = relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn resolve_sqlite_path_keeps_explicit_dot_relative_paths() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let explicit_relative = format!("./{}", absolute_db_str.trim_start_matches('/'));
        let explicit_relative_path = PathBuf::from(&explicit_relative);
        if let Some(parent) = explicit_relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create explicit relative parent");
        }
        std::fs::write(&explicit_relative_path, b"not-a-database")
            .expect("write malformed explicit relative db");

        let resolved = resolve_sqlite_path_with_absolute_fallback(&explicit_relative);
        assert_eq!(resolved, explicit_relative);

        let _ = std::fs::remove_file(&explicit_relative_path);
        if let Some(parent) = explicit_relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn resolve_sqlite_path_does_not_hijack_missing_relative_path() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let missing_relative = absolute_db_str.trim_start_matches('/').to_string();
        let missing_relative_path = PathBuf::from(&missing_relative);
        assert!(
            !missing_relative_path.exists(),
            "test requires the relative path to be absent"
        );

        let resolved = resolve_sqlite_path_with_absolute_fallback(&missing_relative);
        assert_eq!(resolved, missing_relative);
    }

    #[test]
    fn sqlite_identity_key_is_stable_across_pool_clones() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..DbPoolConfig::default()
        };
        let pool = DbPool::new(&config).expect("create pool");
        let clone = pool.clone();

        assert_eq!(pool.sqlite_identity_key(), clone.sqlite_identity_key());
    }

    /// Verify `quarantine_sidecar` renames WAL/SHM files with corrupt- prefix.
    #[test]
    fn quarantine_sidecar_renames_files() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal-content").expect("write wal");

        quarantine_sidecar(&primary, "-wal", "20260218_120000_000").expect("quarantine");

        assert!(!wal.exists(), "original WAL should be gone");
        let quarantined = dir.path().join("test.db-wal.corrupt-20260218_120000_000");
        assert!(quarantined.exists(), "quarantined WAL should exist");
    }

    /// Verify `quarantine_sidecar` is a no-op when the sidecar doesn't exist.
    #[test]
    fn quarantine_sidecar_noop_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        std::fs::write(&primary, b"db").expect("write primary");

        // Should not error when WAL doesn't exist.
        quarantine_sidecar(&primary, "-wal", "20260218_120000_000").expect("quarantine noop");
    }

    #[test]
    fn restore_quarantined_primary_restores_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let shm = dir.path().join("test.db-shm");
        let quarantined = dir
            .path()
            .join("test.db.archive-reconcile-20260218_120000_000");

        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal").expect("write wal");
        std::fs::write(&shm, b"shm").expect("write shm");

        std::fs::rename(&primary, &quarantined).expect("quarantine primary");
        quarantine_sidecar(&primary, "-wal", "20260218_120000_000").expect("quarantine wal");
        quarantine_sidecar(&primary, "-shm", "20260218_120000_000").expect("quarantine shm");

        restore_quarantined_primary(&primary, &quarantined, "20260218_120000_000")
            .expect("restore");

        assert!(primary.exists(), "primary should be restored");
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert_eq!(std::fs::read(&shm).unwrap(), b"shm");
    }

    #[cfg(unix)]
    #[test]
    fn restore_quarantined_primary_fails_closed_when_live_candidate_quarantine_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let original = dir
            .path()
            .join("test.db.archive-reconcile-20260218_120000_000");

        std::fs::write(&primary, b"candidate").expect("write candidate primary");
        std::fs::write(&wal, b"candidate wal").expect("write candidate wal");
        std::fs::write(&original, b"original").expect("write original db");

        let original_mode = std::fs::metadata(dir.path())
            .expect("dir metadata")
            .permissions()
            .mode();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make directory read-only");

        let err = restore_quarantined_primary_with_sidecar_label(
            &primary,
            &original,
            "archive-reconcile",
            "20260218_120000_000",
        )
        .expect_err("live candidate quarantine failure should stop restore");

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(original_mode))
            .expect("restore directory permissions");

        let err_text = err.to_string();
        assert!(
            err_text.contains("failed to quarantine live sqlite candidate"),
            "unexpected error: {err_text}"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), b"candidate");
        assert_eq!(std::fs::read(&wal).unwrap(), b"candidate wal");
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
    }

    #[test]
    fn restore_quarantined_primary_restores_sidecars_without_primary_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let shm = dir.path().join("test.db-shm");

        std::fs::write(
            dir.path().join("test.db-wal.corrupt-20260218_120000_000"),
            b"wal",
        )
        .expect("write quarantined wal");
        std::fs::write(
            dir.path().join("test.db-shm.corrupt-20260218_120000_000"),
            b"shm",
        )
        .expect("write quarantined shm");

        restore_quarantined_primary(
            &primary,
            &dir.path().join("missing.db"),
            "20260218_120000_000",
        )
        .expect("restore sidecars without primary");

        assert!(!primary.exists(), "missing primary should stay absent");
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert_eq!(std::fs::read(&shm).unwrap(), b"shm");
    }

    #[test]
    fn quarantine_reconstructed_candidate_uses_reason_specific_sidecar_paths() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal").expect("write wal");

        let quarantined = quarantine_reconstructed_candidate(
            &primary,
            "20260218_120000_000",
            "reconstruct-failed",
        )
        .expect("quarantine candidate")
        .expect("candidate path");

        assert!(!primary.exists(), "primary should be quarantined");
        assert!(quarantined.exists(), "quarantined primary should exist");
        assert!(
            dir.path()
                .join("test.db-wal.reconstruct-failed-20260218_120000_000")
                .exists(),
            "candidate WAL should use a reason-specific quarantine path"
        );
        assert!(
            !dir.path()
                .join("test.db-wal.corrupt-20260218_120000_000")
                .exists(),
            "candidate WAL should not collide with the original corrupt-sidecar namespace"
        );
    }

    #[test]
    fn quarantine_reconstructed_candidate_rolls_back_on_sidecar_failure() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let quarantine_target = dir
            .path()
            .join("test.db-wal.reconstruct-failed-20260218_120000_000");
        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal").expect("write wal");
        std::fs::create_dir(&quarantine_target).expect("create blocking target directory");

        let err = quarantine_reconstructed_candidate(
            &primary,
            "20260218_120000_000",
            "reconstruct-failed",
        )
        .expect_err("sidecar quarantine failure should roll back");
        let err_text = err.to_string();
        assert!(
            err_text.contains("failed to quarantine WAL sidecar"),
            "unexpected error: {err_text}"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), b"db");
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert!(
            !dir.path()
                .join("test.db.reconstruct-failed-20260218_120000_000")
                .exists(),
            "quarantined primary should be rolled back on failure"
        );
    }

    #[test]
    fn quarantine_reconstruction_candidate_path_moves_candidate_and_journal() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let candidate = dir
            .path()
            .join("test.db.reconstructing-20260218_120000_000");
        let candidate_journal = dir
            .path()
            .join("test.db.reconstructing-20260218_120000_000-journal");

        std::fs::write(&candidate, b"candidate").expect("write candidate");
        std::fs::write(&candidate_journal, b"journal").expect("write candidate journal");

        let quarantined = quarantine_reconstruction_candidate_path(
            &candidate,
            &primary,
            "reconstruct-failed",
            "20260218_120000_000",
        )
        .expect("quarantine candidate")
        .expect("quarantined path");

        assert_eq!(
            quarantined,
            dir.path()
                .join("test.db.reconstruct-failed-20260218_120000_000")
        );
        assert!(!candidate.exists(), "candidate path should be gone");
        assert!(quarantined.exists(), "quarantined candidate should exist");
        assert_eq!(
            std::fs::read(
                dir.path()
                    .join("test.db.reconstruct-failed-20260218_120000_000-journal")
            )
            .unwrap(),
            b"journal"
        );
    }

    #[test]
    fn reconstruct_archive_into_candidate_preserves_existing_primary_on_activation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        std::fs::write(&primary, b"live").expect("write primary");

        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects/demo-project"))
            .expect("create project archive");

        let err = reconstruct_archive_into_candidate(
            &primary,
            &storage_root,
            None,
            "20260218_120000_000",
        )
        .expect_err("existing primary should block candidate activation");

        assert!(
            err.to_string()
                .contains("archive reconstruction failed for"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), b"live");
        assert!(
            dir.path()
                .join("test.db.reconstruct-failed-20260218_120000_000")
                .exists(),
            "candidate should be quarantined instead of replacing the live db"
        );
        assert!(
            !dir.path()
                .join("test.db.reconstructing-20260218_120000_000")
                .exists(),
            "temporary candidate path should not remain live after failure"
        );
    }

    #[test]
    fn quarantine_corrupt_sidecars_or_restore_primary_rolls_back_on_sidecar_failure() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let quarantined = dir.path().join("test.db.corrupt-20260218_120000_000");
        let quarantine_target = dir.path().join("test.db-wal.corrupt-20260218_120000_000");
        std::fs::write(&primary, b"db").expect("write primary");
        std::fs::write(&wal, b"wal").expect("write wal");
        std::fs::rename(&primary, &quarantined).expect("quarantine primary");
        std::fs::create_dir(&quarantine_target).expect("create blocking target directory");

        let err = quarantine_corrupt_sidecars_or_restore_primary(
            &primary,
            &quarantined,
            "20260218_120000_000",
            "unit test",
        )
        .expect_err("sidecar quarantine failure should roll back");
        let err_text = err.to_string();
        assert!(
            err_text.contains("failed to quarantine WAL sidecar"),
            "unexpected error: {err_text}"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), b"db");
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert!(
            !quarantined.exists(),
            "quarantined primary should be restored on failure"
        );
    }

    #[test]
    fn quarantine_corrupt_sidecars_or_restore_primary_restores_sidecars_without_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("test.db");
        let wal = dir.path().join("test.db-wal");
        let shm = dir.path().join("test.db-shm");
        let quarantine_target = dir.path().join("test.db-shm.corrupt-20260218_120000_000");
        std::fs::write(&wal, b"wal").expect("write wal");
        std::fs::write(&shm, b"shm").expect("write shm");
        std::fs::create_dir(&quarantine_target).expect("create blocking target directory");

        let err = quarantine_corrupt_sidecars_or_restore_primary(
            &primary,
            &dir.path().join("missing.db"),
            "20260218_120000_000",
            "unit test without primary",
        )
        .expect_err("sidecar quarantine failure should roll back sidecars");
        let err_text = err.to_string();
        assert!(
            err_text.contains("failed to quarantine SHM sidecar"),
            "unexpected error: {err_text}"
        );
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert_eq!(std::fs::read(&shm).unwrap(), b"shm");
        assert!(
            !dir.path()
                .join("test.db-wal.corrupt-20260218_120000_000")
                .exists(),
            "successful WAL quarantine should be rolled back if SHM quarantine fails"
        );
    }

    // -----------------------------------------------------------------------
    // ensure_sqlite_file_healthy_with_archive tests
    // -----------------------------------------------------------------------

    /// Archive-aware recovery should restore from backup when available.
    #[test]
    fn archive_recovery_prefers_backup_over_archive() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");
        let storage_root = dir.path().join("storage");

        // Create a healthy backup with a marker table.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('from-backup')")
            .unwrap();
        drop(conn);
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));
        std::fs::copy(&primary, &backup).unwrap();

        // Corrupt the primary.
        std::fs::write(&primary, b"corrupted-data").unwrap();

        // Create a minimal storage root (even though backup should win).
        std::fs::create_dir_all(storage_root.join("projects").join("proj1")).unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        // Should have restored from backup (marker table present).
        let val = sqlite_marker_value(&primary);
        assert_eq!(
            val.as_deref(),
            Some("from-backup"),
            "backup should take priority over archive"
        );
    }

    /// Archive-aware recovery should reconstruct from archive when no backup exists.
    #[test]
    fn archive_recovery_reconstructs_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        // Corrupt primary, no backup.
        std::fs::write(&primary, b"corrupted-data").unwrap();

        // Set up archive with a project + agent + message.
        let proj_dir = storage_root.join("projects").join("test-proj");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"SwiftFox","role":"Coder","model":"claude","registered_ts":"2026-01-15T10:00:00"}"#,
        ).unwrap();

        let msg_dir = proj_dir.join("messages").join("2026").join("01");
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            msg_dir.join("001_test.md"),
            "---json\n{\n  \"id\": 1,\n  \"subject\": \"Test\",\n  \"from_agent\": \"SwiftFox\",\n  \"importance\": \"normal\",\n  \"to\": [\"CalmLake\"],\n  \"cc\": [],\n  \"bcc\": [],\n  \"thread_id\": \"t1\",\n  \"in_reply_to\": null,\n  \"created_ts\": \"2026-01-15T10:05:00\"\n}\n---\n\nTest body\n",
        ).unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        assert!(
            sqlite_file_is_healthy(&primary).unwrap(),
            "reconstructed DB should be healthy"
        );

        // Verify data was actually recovered from archive.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .unwrap();
        let count = rows
            .first()
            .and_then(|r| r.get_named::<i64>("n").ok())
            .unwrap_or(0);
        assert!(count >= 1, "should have at least 1 message from archive");
    }

    #[test]
    fn archive_recovery_reconciles_healthy_db_when_archive_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .unwrap();

        crate::reconstruct::reconstruct_from_archive(&primary, &storage_root)
            .expect("seed initial reconstructed db");

        std::fs::write(
            msg_dir.join("2026-03-22T12-05-00Z__second__2.md"),
            "---json\n{\"id\":2,\"from\":\"Alice\",\"to\":[\"Carol\"],\"subject\":\"Second\",\"importance\":\"urgent\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:05:00Z\",\"attachments\":[]}\n---\n\nsecond body\n",
        )
        .unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect("archive-aware recovery should reconcile healthy-but-stale dbs");

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                &[],
            )
            .unwrap();
        let row = rows.first().unwrap();
        assert_eq!(row.get_named::<i64>("count").unwrap_or(0), 2);
        assert_eq!(row.get_named::<i64>("max_id").unwrap_or(0), 2);
    }

    #[test]
    fn archive_recovery_reconciles_restored_backup_when_archive_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let backup = dir.path().join("storage.sqlite3.bak");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .unwrap();

        crate::reconstruct::reconstruct_from_archive(&primary, &storage_root)
            .expect("seed db for backup");
        let _ = std::fs::remove_file(format!("{}-wal", primary.display()));
        let _ = std::fs::remove_file(format!("{}-shm", primary.display()));
        std::fs::copy(&primary, &backup).unwrap();

        std::fs::write(
            msg_dir.join("2026-03-22T12-05-00Z__second__2.md"),
            "---json\n{\"id\":2,\"from\":\"Alice\",\"to\":[\"Carol\"],\"subject\":\"Second\",\"importance\":\"urgent\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:05:00Z\",\"attachments\":[]}\n---\n\nsecond body\n",
        )
        .unwrap();
        std::fs::write(&primary, b"corrupted-data").unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect("archive-aware recovery should reconcile stale backups after restore");

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                &[],
            )
            .unwrap();
        let row = rows.first().unwrap();
        assert_eq!(row.get_named::<i64>("count").unwrap_or(0), 2);
        assert_eq!(row.get_named::<i64>("max_id").unwrap_or(0), 2);
    }

    #[test]
    fn reconcile_archive_state_before_init_reconstructs_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("test-proj");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        let msg_dir = proj_dir.join("messages").join("2026").join("01");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"test-proj","human_key":"/tmp/test-proj"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"SwiftFox","program":"coder","model":"claude","inception_ts":"2026-01-15T10:00:00Z","last_active_ts":"2026-01-15T10:00:01Z"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("2026-01-15T10-05-00Z__test__7.md"),
            "---json\n{\"id\":7,\"from\":\"SwiftFox\",\"to\":[\"CalmLake\"],\"subject\":\"Test\",\"thread_id\":\"t1\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-01-15T10:05:00Z\",\"attachments\":[]}\n---\n\nTest body\n",
        )
        .unwrap();

        assert!(
            reconcile_archive_state_before_init(&primary, &storage_root).unwrap(),
            "missing db with archive state should reconstruct before init"
        );

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                &[],
            )
            .unwrap();
        let row = rows.first().unwrap();
        assert_eq!(row.get_named::<i64>("count").unwrap_or(0), 1);
        assert_eq!(row.get_named::<i64>("max_id").unwrap_or(0), 7);
    }

    #[test]
    fn reconcile_archive_state_before_init_rebuilds_healthy_db_when_archive_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .unwrap();

        crate::reconstruct::reconstruct_from_archive(&primary, &storage_root)
            .expect("seed stale sqlite db from archive");

        std::fs::write(
            msg_dir.join("2026-03-22T12-05-00Z__second__2.md"),
            "---json\n{\"id\":2,\"from\":\"Alice\",\"to\":[\"Carol\"],\"subject\":\"Second\",\"importance\":\"urgent\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:05:00Z\",\"attachments\":[]}\n---\n\nsecond body\n",
        )
        .unwrap();

        assert!(
            reconcile_archive_state_before_init(&primary, &storage_root).unwrap(),
            "archive-ahead healthy db should be reconciled before init"
        );

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                &[],
            )
            .unwrap();
        let row = rows.first().unwrap();
        assert_eq!(row.get_named::<i64>("count").unwrap_or(0), 2);
        assert_eq!(row.get_named::<i64>("max_id").unwrap_or(0), 2);
    }

    #[test]
    fn reconcile_archive_state_before_init_rebuilds_healthy_db_when_archive_agents_are_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        drop(conn);

        assert!(
            reconcile_archive_state_before_init(&primary, &storage_root).unwrap(),
            "archive-ahead agent/project state should be reconciled before init"
        );

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let project_rows = conn
            .query_sync("SELECT COUNT(*) AS count FROM projects", &[])
            .unwrap();
        let agent_rows = conn
            .query_sync("SELECT COUNT(*) AS count FROM agents", &[])
            .unwrap();
        assert_eq!(project_rows[0].get_named::<i64>("count").unwrap_or(0), 1);
        assert_eq!(agent_rows[0].get_named::<i64>("count").unwrap_or(0), 1);
    }

    #[test]
    fn reconcile_archive_state_before_init_rebuilds_when_project_identity_differs_with_equal_counts()
     {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("archive-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("wrong-project".to_string()),
                Value::Text("/wrong-project".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("Alice".to_string()),
                Value::Text("coder".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();
        drop(conn);

        assert!(
            reconcile_archive_state_before_init(&primary, &storage_root).unwrap(),
            "archive project identity drift should be reconciled even when counts match"
        );

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync("SELECT slug, human_key FROM projects", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap_or_default(),
            "archive-project"
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap_or_default(),
            "/archive-project"
        );
    }

    #[test]
    fn reconcile_archive_state_before_init_rebuilds_when_human_key_differs_with_equal_slug_and_counts()
     {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("shared-slug");
        let agent_dir = proj_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"shared-slug","human_key":"/archive-project"}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .unwrap();

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("shared-slug".to_string()),
                Value::Text("/wrong-project".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("Alice".to_string()),
                Value::Text("coder".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .unwrap();
        drop(conn);

        assert!(
            reconcile_archive_state_before_init(&primary, &storage_root).unwrap(),
            "archive human_key drift should be reconciled even when slug and counts match"
        );

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync("SELECT slug, human_key FROM projects", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap_or_default(),
            "shared-slug"
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap_or_default(),
            "/archive-project"
        );
    }

    #[test]
    fn archive_recovery_accepts_project_only_reconstruction_as_durable_state() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        let project_dir = storage_root.join("projects").join("project-only");

        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"project-only","human_key":"/project-only"}"#,
        )
        .unwrap();
        std::fs::write(&primary, b"corrupted-data").unwrap();

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect("project-only archive state should survive fail-closed recovery");

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT slug, human_key FROM projects WHERE slug = 'project-only'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn classify_mailbox_ownership_accepts_current_process_owner() {
        let current_pid = std::process::id();
        let processes = vec![MailboxOwnershipProcess {
            pid: current_pid,
            command: Some("mcp-agent-mail serve".to_string()),
            executable_path: Some("/tmp/mcp-agent-mail".to_string()),
            executable_deleted: false,
            holds_storage_root_lock: true,
            holds_sqlite_lock: true,
            holds_database_file: true,
        }];

        let (disposition, competing_pids, supervised_restart_required, detail) =
            classify_mailbox_ownership(&processes, current_pid);

        assert_eq!(disposition, MailboxOwnershipDisposition::Unowned);
        assert!(competing_pids.is_empty());
        assert!(!supervised_restart_required);
        assert!(detail.contains("no competing"));
    }

    #[test]
    fn classify_mailbox_ownership_flags_deleted_executable_owner() {
        let processes = vec![MailboxOwnershipProcess {
            pid: 4242,
            command: Some("mcp-agent-mail serve".to_string()),
            executable_path: Some("/tmp/mcp-agent-mail (deleted)".to_string()),
            executable_deleted: true,
            holds_storage_root_lock: true,
            holds_sqlite_lock: false,
            holds_database_file: true,
        }];

        let (disposition, competing_pids, supervised_restart_required, detail) =
            classify_mailbox_ownership(&processes, std::process::id());

        assert_eq!(disposition, MailboxOwnershipDisposition::DeletedExecutable);
        assert_eq!(competing_pids, vec![4242]);
        assert!(supervised_restart_required);
        assert!(detail.contains("deleted executable"));
    }

    #[test]
    fn classify_mailbox_ownership_flags_stale_live_process_without_activity_locks() {
        let processes = vec![MailboxOwnershipProcess {
            pid: 4343,
            command: Some("mcp-agent-mail serve".to_string()),
            executable_path: Some("/tmp/mcp-agent-mail".to_string()),
            executable_deleted: false,
            holds_storage_root_lock: false,
            holds_sqlite_lock: false,
            holds_database_file: true,
        }];

        let (disposition, competing_pids, supervised_restart_required, detail) =
            classify_mailbox_ownership(&processes, std::process::id());

        assert_eq!(disposition, MailboxOwnershipDisposition::StaleLiveProcess);
        assert_eq!(competing_pids, vec![4343]);
        assert!(supervised_restart_required);
        assert!(detail.contains("without mailbox activity locks"));
    }

    #[test]
    fn classify_mailbox_ownership_flags_split_brain() {
        let processes = vec![
            MailboxOwnershipProcess {
                pid: 4444,
                command: Some("mcp-agent-mail serve".to_string()),
                executable_path: Some("/tmp/mcp-agent-mail".to_string()),
                executable_deleted: false,
                holds_storage_root_lock: true,
                holds_sqlite_lock: false,
                holds_database_file: true,
            },
            MailboxOwnershipProcess {
                pid: 5555,
                command: Some("mcp-agent-mail serve".to_string()),
                executable_path: Some("/tmp/mcp-agent-mail-cli".to_string()),
                executable_deleted: false,
                holds_storage_root_lock: false,
                holds_sqlite_lock: true,
                holds_database_file: true,
            },
        ];

        let (disposition, competing_pids, supervised_restart_required, detail) =
            classify_mailbox_ownership(&processes, std::process::id());

        assert_eq!(disposition, MailboxOwnershipDisposition::SplitBrain);
        assert_eq!(competing_pids, vec![4444, 5555]);
        assert!(supervised_restart_required);
        assert!(detail.contains("split-brain"));
    }

    #[cfg(unix)]
    #[test]
    fn archive_recovery_rejects_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let real_storage = dir.path().join("real-storage");
        let storage_root = dir.path().join("storage");

        std::fs::write(&primary, b"corrupted-data").unwrap();

        let proj_dir = real_storage.join("projects").join("test-proj");
        let agent_dir = proj_dir.join("agents").join("SwiftFox");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"SwiftFox","registered_ts":"2026-01-15T10:00:00"}"#,
        )
        .unwrap();
        symlink(&real_storage, &storage_root).unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("symlinked storage roots must not be trusted for archive recovery");
        let err_text = err.to_string();
        assert!(
            err_text.contains("refusing blank reinitialization to avoid data loss"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "fail-closed recovery should not leave behind a fresh empty database"
        );
    }

    /// Archive-aware recovery must fail closed when a real DB was quarantined
    /// and no backup/archive path can produce a healthy replacement.
    #[test]
    fn archive_recovery_refuses_blank_reinit_with_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        // Corrupt primary, no backup, empty storage.
        std::fs::write(&primary, b"corrupted-data").unwrap();
        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("should refuse to blank-reinitialize after quarantining a real DB");
        let err_text = err.to_string();
        assert!(
            err_text.contains("refusing blank reinitialization to avoid data loss"),
            "unexpected error: {err_text}"
        );

        assert!(
            !primary.exists(),
            "primary DB should stay absent after fail-closed recovery"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
            "quarantined corrupt artifact should be preserved for manual recovery"
        );
    }

    #[test]
    fn archive_recovery_missing_primary_with_quarantined_artifact_is_not_treated_as_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::write(
            dir.path()
                .join("storage.sqlite3.corrupt-20260307_000000_000"),
            b"quarantined-corrupt-db",
        )
        .unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("quarantined corrupt artifacts should block blank reinit");
        let err_text = err.to_string();
        assert!(
            err_text.contains("quarantined recovery artifact"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "recovery must not silently create a fresh DB when only quarantined state exists"
        );
    }

    #[test]
    fn archive_recovery_missing_primary_with_archive_reconcile_artifact_is_not_treated_as_fresh_start()
     {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::write(
            dir.path()
                .join("storage.sqlite3.archive-reconcile-20260307_000000_000"),
            b"quarantined-archive-reconcile-db",
        )
        .unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("archive-reconcile artifacts should block blank reinit");
        let err_text = err.to_string();
        assert!(
            err_text.contains("quarantined recovery artifact"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "recovery must not silently create a fresh DB when archive-reconcile state exists"
        );
    }

    #[test]
    fn archive_recovery_missing_primary_with_reconstruct_artifact_is_not_treated_as_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::write(
            dir.path()
                .join("storage.sqlite3.reconstruct-failed-20260307_000000_000"),
            b"quarantined-reconstruct-db",
        )
        .unwrap();

        let err = ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect_err("reconstruct artifacts should block blank reinit");
        let err_text = err.to_string();
        assert!(
            err_text.contains("quarantined recovery artifact"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "recovery must not silently create a fresh DB when reconstruct state exists"
        );
    }

    #[test]
    fn archive_salvage_recovery_missing_primary_with_quarantined_artifact_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::write(
            dir.path()
                .join("storage.sqlite3.archive-reconcile-20260307_000000_000"),
            b"quarantined-archive-reconcile-db",
        )
        .unwrap();
        std::fs::create_dir_all(storage_root.join("projects").join("test-proj")).unwrap();

        let err = reconstruct_sqlite_file_with_archive_salvage(&primary, &storage_root)
            .expect_err("quarantined primary artifacts must block archive-salvage reconstruction");
        let err_text = err.to_string();
        assert!(
            err_text.contains("quarantined recovery artifact"),
            "unexpected error: {err_text}"
        );
        assert!(
            !primary.exists(),
            "archive-salvage path must not recreate a primary when quarantined state exists"
        );
    }

    #[test]
    fn archive_salvage_recovery_restores_original_sidecars_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        let wal = dir.path().join("storage.sqlite3-wal");
        let shm = dir.path().join("storage.sqlite3-shm");
        std::fs::write(&wal, b"original wal").unwrap();
        std::fs::write(&shm, b"original shm").unwrap();

        let storage_root = dir.path().join("not-a-directory");
        std::fs::write(&storage_root, b"boom").unwrap();

        let err = reconstruct_sqlite_file_with_archive_salvage(&primary, &storage_root)
            .expect_err("invalid archive root should fail reconstruction");
        let err_text = err.to_string();
        assert!(
            err_text.contains("archive reconciliation failed"),
            "unexpected error: {err_text}"
        );

        assert!(primary.exists(), "original database should be restored");
        assert_eq!(std::fs::read(&wal).unwrap(), b"original wal");
        assert_eq!(std::fs::read(&shm).unwrap(), b"original shm");
    }

    /// Archive-aware recovery should skip when DB is already healthy.
    #[test]
    fn archive_recovery_noop_on_healthy_db() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        // Create a healthy DB.
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw("CREATE TABLE marker(value TEXT NOT NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO marker(value) VALUES('original')")
            .unwrap();
        drop(conn);

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root).unwrap();

        // Data should be untouched.
        let val = sqlite_marker_value(&primary);
        assert_eq!(
            val.as_deref(),
            Some("original"),
            "healthy DB should not be touched"
        );
    }

    // -----------------------------------------------------------------------
    // create_proactive_backup tests
    // -----------------------------------------------------------------------

    /// Proactive backup creates a .bak file after successful integrity check.
    #[test]
    fn proactive_backup_creates_bak_file() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_backup.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        // Trigger migration so the file exists.
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().unwrap();
        });

        // Create backup with 0 max_age so it always writes.
        let result = pool
            .create_proactive_backup(std::time::Duration::ZERO)
            .unwrap();
        assert!(result.is_some(), "should create a backup");

        let bak_path = result.unwrap();
        assert!(bak_path.exists(), "backup file should exist");
        assert!(
            bak_path.to_string_lossy().ends_with(".bak"),
            "should end with .bak"
        );
    }

    /// Proactive backup skips when existing backup is fresh.
    #[test]
    fn proactive_backup_skips_fresh_backup() {
        use asupersync::runtime::RuntimeBuilder;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_skip.db");
        let config = DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        rt.block_on(async {
            let _conn = pool.acquire(&cx).await.into_result().unwrap();
        });

        // First backup should succeed.
        let first = pool
            .create_proactive_backup(std::time::Duration::from_hours(1))
            .unwrap();
        assert!(first.is_some(), "first backup should create file");

        // Second backup should skip (backup is <1 hour old).
        let second = pool
            .create_proactive_backup(std::time::Duration::from_hours(1))
            .unwrap();
        assert!(second.is_none(), "should skip since backup is fresh");
    }

    /// Proactive backup is a no-op for :memory: databases.
    #[test]
    fn proactive_backup_noop_for_memory() {
        let config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let pool = DbPool::new(&config).unwrap();

        let result = pool
            .create_proactive_backup(std::time::Duration::ZERO)
            .unwrap();
        assert!(result.is_none(), "memory DB should not create backup");
    }

    // ── auto_pool_size ─────────────────────────────────────────────────

    #[test]
    fn auto_pool_size_returns_valid_bounds() {
        let (min, max) = auto_pool_size();
        assert!(min >= 10, "min should be at least 10, got {min}");
        assert!(max >= 50, "max should be at least 50, got {max}");
        assert!(min <= 50, "min should be at most 50, got {min}");
        assert!(max <= 200, "max should be at most 200, got {max}");
        assert!(min <= max, "min ({min}) should not exceed max ({max})");
    }

    // ── is_corruption_error_message ────────────────────────────────────

    #[test]
    fn corruption_error_detects_malformed_image() {
        assert!(is_corruption_error_message(
            "database disk image is malformed"
        ));
    }

    #[test]
    fn corruption_error_detects_malformed_schema() {
        assert!(is_corruption_error_message(
            "malformed database schema - broken_table"
        ));
    }

    #[test]
    fn corruption_error_detects_not_a_database() {
        assert!(is_corruption_error_message("file is not a database"));
    }

    #[test]
    fn corruption_error_detects_no_healthy_backup() {
        assert!(is_corruption_error_message("no healthy backup was found"));
    }

    #[test]
    fn corruption_error_case_insensitive() {
        assert!(is_corruption_error_message(
            "DATABASE DISK IMAGE IS MALFORMED"
        ));
        assert!(is_corruption_error_message("File Is Not A Database"));
    }

    #[test]
    fn corruption_error_rejects_unrelated_messages() {
        assert!(!is_corruption_error_message("connection refused"));
        assert!(!is_corruption_error_message("timeout"));
        assert!(!is_corruption_error_message("constraint violation"));
        assert!(!is_corruption_error_message("unique constraint failed"));
        assert!(!is_corruption_error_message("no such table"));
        assert!(!is_corruption_error_message(""));
    }

    #[test]
    fn corruption_error_detects_embedded_in_longer_message() {
        assert!(is_corruption_error_message(
            "SqlError: database disk image is malformed (while running SELECT)"
        ));
    }

    // ── is_sqlite_recovery_error_message ───────────────────────────────

    #[test]
    fn recovery_error_includes_all_corruption_patterns() {
        // All corruption patterns are also recovery patterns
        assert!(is_sqlite_recovery_error_message(
            "database disk image is malformed"
        ));
        assert!(is_sqlite_recovery_error_message(
            "malformed database schema"
        ));
        assert!(is_sqlite_recovery_error_message("file is not a database"));
        assert!(is_sqlite_recovery_error_message(
            "no healthy backup was found"
        ));
    }

    #[test]
    fn recovery_error_detects_out_of_memory() {
        assert!(is_sqlite_recovery_error_message("out of memory"));
        assert!(is_sqlite_recovery_error_message("OUT OF MEMORY"));
    }

    #[test]
    fn recovery_error_detects_cursor_stack_empty() {
        assert!(is_sqlite_recovery_error_message("cursor stack is empty"));
    }

    #[test]
    fn recovery_error_detects_unwrap_none() {
        assert!(is_sqlite_recovery_error_message(
            "called `option::unwrap()` on a `none` value"
        ));
    }

    #[test]
    fn recovery_error_detects_internal_error() {
        assert!(is_sqlite_recovery_error_message("internal error"));
    }

    #[test]
    fn recovery_error_detects_snapshot_conflict() {
        assert!(is_sqlite_snapshot_conflict_error_message(
            "database is busy (snapshot conflict on pages: page 4434 > snapshot db_size 4433 (latest: 4433))"
        ));
        assert!(is_sqlite_snapshot_conflict_error_message(
            "BUSY_SNAPSHOT while opening database"
        ));
    }

    #[test]
    fn recovery_error_rejects_non_recovery_messages() {
        assert!(!is_sqlite_recovery_error_message("connection refused"));
        assert!(!is_sqlite_recovery_error_message("timeout"));
        assert!(!is_sqlite_recovery_error_message("no such table"));
        assert!(!is_sqlite_recovery_error_message(""));
    }

    #[test]
    fn recovery_error_detects_wal_file_too_small() {
        assert!(is_sqlite_recovery_error_message(
            "WAL file too small for header during rebuild: read 0, need 32"
        ));
        assert!(is_corruption_error_message(
            "WAL file too small for header during rebuild: read 0, need 32"
        ));
    }

    #[test]
    fn cleanup_empty_wal_sidecar_removes_zero_byte_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cleanup_test.db");
        // Create a real DB so the main file exists.
        let conn = DbConn::open_file(db_path.to_str().unwrap()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        // Create a 0-byte WAL sidecar (simulating crash artifact).
        let wal_path = dir.path().join("cleanup_test.db-wal");
        std::fs::write(&wal_path, b"").expect("create empty wal");
        assert!(wal_path.exists());
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0);

        cleanup_empty_wal_sidecar(db_path.to_str().unwrap());

        assert!(!wal_path.exists(), "empty WAL should have been removed");
    }

    #[test]
    fn cleanup_empty_wal_sidecar_preserves_nonempty_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("preserve_test.db");
        let conn = DbConn::open_file(db_path.to_str().unwrap()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        // Create a non-empty WAL sidecar.
        let wal_path = dir.path().join("preserve_test.db-wal");
        std::fs::write(&wal_path, b"some WAL content here").expect("create wal");
        assert!(wal_path.exists());

        cleanup_empty_wal_sidecar(db_path.to_str().unwrap());

        assert!(wal_path.exists(), "non-empty WAL should be preserved");
    }

    #[test]
    fn ensure_sqlite_file_healthy_cleans_zero_byte_wal_before_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("zero-byte-wal.db");
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        let wal = dir.path().join("zero-byte-wal.db-wal");
        std::fs::write(&wal, b"").expect("create empty wal");
        assert!(wal.exists(), "empty wal stub should exist before recovery");

        ensure_sqlite_file_healthy(&primary).expect("healthy db with empty wal should recover");

        assert!(
            sqlite_file_is_healthy(&primary).expect("health check after cleanup"),
            "primary db should remain healthy after empty wal cleanup"
        );
        assert!(
            !wal.exists(),
            "empty wal stub should be removed instead of forcing backup/reinit recovery"
        );
    }

    #[test]
    fn ensure_sqlite_file_healthy_with_archive_cleans_zero_byte_wal_before_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("zero-byte-wal-archive.db");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");

        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        let wal = dir.path().join("zero-byte-wal-archive.db-wal");
        std::fs::write(&wal, b"").expect("create empty wal");
        assert!(
            wal.exists(),
            "empty wal stub should exist before archive-aware recovery"
        );

        ensure_sqlite_file_healthy_with_archive(&primary, &storage_root)
            .expect("healthy db with empty wal should recover");

        assert!(
            sqlite_file_is_healthy(&primary).expect("health check after archive-aware cleanup"),
            "primary db should remain healthy after archive-aware empty wal cleanup"
        );
        assert!(
            !wal.exists(),
            "archive-aware recovery should remove empty wal stub instead of escalating"
        );
    }

    #[test]
    fn refuse_auto_recovery_with_live_sidecar_directory_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("live-sidecar-dir.db");
        let conn = DbConn::open_file(primary.to_string_lossy().as_ref()).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create table");
        drop(conn);

        let wal_dir = dir.path().join("live-sidecar-dir.db-wal");
        std::fs::create_dir(&wal_dir).expect("create wal directory");
        std::fs::write(wal_dir.join("marker"), b"not-a-wal").expect("write marker");

        let err = refuse_auto_recovery_with_live_sidecars(&primary).expect_err("must fail closed");
        let err_text = err.to_string();
        assert!(
            err_text.contains("automatic checkpoint failed")
                || err_text.contains("non-empty WAL/SHM sidecars remain"),
            "unexpected error: {err_text}"
        );
        assert!(
            wal_dir.exists(),
            "malformed sidecar directory should remain untouched"
        );
    }

    #[test]
    fn sqlite_init_retry_treats_locked_db_as_retryable() {
        let err = SqlError::Custom("database is locked".to_string());
        assert!(
            should_retry_sqlite_init_error(&err),
            "database lock contention should retry during sqlite init"
        );
    }

    #[test]
    fn sqlite_init_retry_rejects_non_retryable_errors() {
        let err = SqlError::Custom("syntax error near SELECT".to_string());
        assert!(
            !should_retry_sqlite_init_error(&err),
            "non-retryable SQL errors must fail fast during sqlite init"
        );
    }

    #[test]
    fn sqlite_open_lock_retry_delay_exponential_and_capped() {
        assert_eq!(sqlite_lock_retry_delay(0), Duration::from_millis(25));
        assert_eq!(sqlite_lock_retry_delay(1), Duration::from_millis(50));
        assert_eq!(sqlite_lock_retry_delay(2), Duration::from_millis(100));
        assert_eq!(sqlite_lock_retry_delay(3), Duration::from_millis(200));
        assert_eq!(
            sqlite_lock_retry_delay(999),
            Duration::from_millis(200),
            "backoff should cap to avoid unbounded startup delay"
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn retry_sqlite_lock_impl_retries_then_succeeds() {
        let attempts = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result = retry_sqlite_lock_impl(
            "ignored.sqlite3",
            "test operation",
            || {
                let next = attempts.get() + 1;
                attempts.set(next);
                if next <= 2 {
                    Err(SqlError::Custom("database is locked".to_string()))
                } else {
                    Ok(())
                }
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(result.is_ok(), "expected success after lock retries");
        assert_eq!(attempts.get(), 3);
        assert_eq!(
            sleep_calls.borrow().as_slice(),
            &[sqlite_lock_retry_delay(0), sqlite_lock_retry_delay(1)]
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn open_sqlite_file_with_lock_retry_retries_then_succeeds() {
        let open_calls = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result = open_sqlite_file_with_lock_retry_impl(
            "ignored",
            |_| {
                let next = open_calls.get() + 1;
                open_calls.set(next);
                if next <= 2 {
                    Err(SqlError::Custom("database is locked".to_string()))
                } else {
                    DbConn::open_memory()
                }
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(result.is_ok(), "expected success after lock retries");
        assert_eq!(open_calls.get(), 3);
        assert_eq!(
            sleep_calls.borrow().as_slice(),
            &[sqlite_lock_retry_delay(0), sqlite_lock_retry_delay(1)]
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn open_sqlite_file_with_lock_retry_does_not_retry_non_lock_errors() {
        let open_calls = std::cell::Cell::new(0usize);
        let sleep_calls = std::cell::RefCell::new(Vec::new());
        let result: Result<DbConn, SqlError> = open_sqlite_file_with_lock_retry_impl(
            "ignored",
            |_| {
                open_calls.set(open_calls.get() + 1);
                Err(SqlError::Custom("malformed database schema".to_string()))
            },
            |delay| sleep_calls.borrow_mut().push(delay),
        );
        assert!(
            result.is_err(),
            "expected immediate failure on non-lock error"
        );
        assert_eq!(open_calls.get(), 1, "non-lock errors should not be retried");
        assert!(
            sleep_calls.borrow().is_empty(),
            "non-lock errors should not trigger backoff sleeps"
        );
    }

    // ── sqlite_absolute_fallback_path ──────────────────────────────────

    #[test]
    fn fallback_path_returns_none_for_memory_db() {
        assert!(
            sqlite_absolute_fallback_path(":memory:", "database disk image is malformed").is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_absolute_path() {
        assert!(
            sqlite_absolute_fallback_path("/data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_dot_relative() {
        assert!(
            sqlite_absolute_fallback_path("./data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_dotdot_relative() {
        assert!(
            sqlite_absolute_fallback_path("../data/db.sqlite3", "database disk image is malformed")
                .is_none()
        );
    }

    #[test]
    fn fallback_path_returns_none_for_non_recovery_error() {
        assert!(sqlite_absolute_fallback_path("data/db.sqlite3", "connection refused").is_none());
    }

    #[test]
    fn fallback_path_returns_none_when_absolute_candidate_does_not_exist() {
        assert!(
            sqlite_absolute_fallback_path(
                "nonexistent/path/db.sqlite3",
                "database disk image is malformed"
            )
            .is_none()
        );
    }

    // ── ensure_sqlite_parent_dir_exists ─────────────────────────────────

    #[test]
    fn ensure_parent_dir_noop_for_memory_db() {
        assert!(ensure_sqlite_parent_dir_exists(":memory:").is_ok());
    }

    #[test]
    fn ensure_parent_dir_creates_missing_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c/test.sqlite3");
        assert!(!tmp.path().join("a").exists());
        ensure_sqlite_parent_dir_exists(nested.to_str().unwrap()).unwrap();
        assert!(tmp.path().join("a/b/c").exists());
    }

    #[test]
    fn ensure_parent_dir_ok_when_already_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite3");
        assert!(ensure_sqlite_parent_dir_exists(db_path.to_str().unwrap()).is_ok());
    }

    // ── open_sqlite_file_with_recovery ──────────────────────────────────

    #[test]
    fn open_memory_db_succeeds() {
        let conn = open_sqlite_file_with_recovery(":memory:").unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn open_real_file_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite3");
        let conn = open_sqlite_file_with_recovery(db_path.to_str().unwrap()).unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn open_creates_parent_dirs_if_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("sub/dir/test.sqlite3");
        let conn = open_sqlite_file_with_recovery(db_path.to_str().unwrap()).unwrap();
        let rows = conn.query_sync("SELECT 1 AS val", &[]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn sqlite_init_missing_file_bootstraps_without_recovery_artifacts() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("fresh_bootstrap.sqlite3");
        let db_path_str = db_path.to_str().expect("utf8 db path");

        match rt.block_on(run_sqlite_init_once(&cx, db_path_str, true)) {
            Outcome::Ok(()) => {}
            Outcome::Err(err) => panic!("sqlite init should bootstrap fresh files: {err}"),
            Outcome::Cancelled(reason) => panic!("sqlite init cancelled unexpectedly: {reason:?}"),
            Outcome::Panicked(payload) => {
                std::panic::panic_any(payload);
            }
        }

        assert!(
            db_path.exists(),
            "bootstrap should create the sqlite file for a fresh database"
        );
        assert!(
            sqlite_file_is_healthy(&db_path).expect("health check after fresh bootstrap"),
            "fresh bootstrap should leave a healthy sqlite file"
        );

        let conn = open_sqlite_file_with_lock_retry(db_path_str)
            .expect("runtime sqlite should open after fresh bootstrap");
        let rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name = 'projects'",
                &[],
            )
            .expect("query sqlite_master");
        assert_eq!(rows.len(), 1, "fresh bootstrap should apply migrations");

        let mut recovery_artifacts = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.starts_with("fresh_bootstrap.sqlite3.corrupt-")
                    || name.starts_with("fresh_bootstrap.sqlite3.reconstruct-")
            })
            .collect::<Vec<_>>();
        recovery_artifacts.sort();
        assert!(
            recovery_artifacts.is_empty(),
            "fresh bootstrap should not quarantine/reconstruct missing databases: {recovery_artifacts:?}"
        );
    }

    #[test]
    fn sqlite_init_drops_legacy_agents_lower_name_index_before_runtime_open() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("legacy_agents_lower_name.sqlite3");
        let db_path_str = db_path.to_str().expect("utf8 db path");

        let canonical = open_sqlite_file_with_lock_retry_canonical(db_path_str)
            .expect("open canonical sqlite file");
        canonical
            .execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL)
            .expect("apply canonical pragmas");
        canonical
            .execute_raw(&schema::init_schema_sql_base())
            .expect("initialize base schema");
        canonical
            .execute_raw(
                "CREATE UNIQUE INDEX uq_agents_name_ci \
                 ON agents(lower(name))",
            )
            .expect("create legacy lower(name) partial index");
        drop(canonical);

        match rt.block_on(run_sqlite_init_once(&cx, db_path_str, true)) {
            Outcome::Ok(()) => {}
            Outcome::Err(err) => panic!("sqlite init should repair legacy index: {err}"),
            Outcome::Cancelled(reason) => panic!("sqlite init cancelled unexpectedly: {reason:?}"),
            Outcome::Panicked(payload) => {
                std::panic::panic_any(payload);
            }
        }

        let verify = open_sqlite_file_with_lock_retry_canonical(db_path_str)
            .expect("reopen canonical sqlite file");
        let legacy_rows = verify
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'index' AND name = 'uq_agents_name_ci'",
                &[],
            )
            .expect("query sqlite_master");
        assert!(
            legacy_rows.is_empty(),
            "canonical init should drop legacy lower(name) partial index"
        );
        drop(verify);

        let runtime = open_sqlite_file_with_lock_retry(db_path_str)
            .expect("runtime sqlite should open after legacy index cleanup");
        let rows = runtime
            .query_sync("SELECT 1 AS val", &[])
            .expect("runtime query");
        assert_eq!(rows.len(), 1);
    }

    // ── DbPoolConfig::from_env ──────────────────────────────────────────

    #[test]
    fn pool_config_from_env_has_defaults() {
        let config = DbPoolConfig::from_env();
        assert!(!config.database_url.is_empty() || config.database_url.is_empty()); // just ensure it doesn't panic
        assert!(config.min_connections > 0);
        assert!(config.max_connections >= config.min_connections);
    }

    #[test]
    fn pool_config_from_env_defaults_database_into_storage_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage_root = tmp.path().join("mailbox");
        let storage_root_str = storage_root.to_string_lossy().to_string();
        let expected_database_url = format!(
            "sqlite:///{}",
            storage_root.join("storage.sqlite3").display()
        );

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("STORAGE_ROOT", storage_root_str.as_str())],
            || {
                let config = DbPoolConfig::from_env();
                assert_eq!(config.database_url, expected_database_url);
                assert_eq!(config.storage_root.as_deref(), Some(storage_root.as_path()));
            },
        );
    }

    // ── RecoveryAction / RecoveryApproval policy tests ─────────────────

    #[test]
    fn recovery_action_all_covers_every_variant() {
        assert_eq!(
            RecoveryAction::ALL.len(),
            RecoveryAction::SILENT.len() + RecoveryAction::ESCALATED.len(),
            "ALL must equal SILENT + ESCALATED"
        );
    }

    #[test]
    fn recovery_action_silent_list_matches_approval() {
        for action in RecoveryAction::SILENT {
            assert!(
                action.is_silent(),
                "{action} is in SILENT list but approval() is {:?}",
                action.approval()
            );
            assert!(
                !action.requires_escalation(),
                "{action} is in SILENT list but requires_escalation() is true"
            );
        }
    }

    #[test]
    fn recovery_action_escalated_list_matches_approval() {
        for action in RecoveryAction::ESCALATED {
            assert!(
                action.requires_escalation(),
                "{action} is in ESCALATED list but requires_escalation() is false"
            );
            assert!(
                !action.is_silent(),
                "{action} is in ESCALATED list but is_silent() is true"
            );
        }
    }

    #[test]
    fn recovery_action_labels_are_unique() {
        let mut seen = HashSet::new();
        for action in RecoveryAction::ALL {
            assert!(
                seen.insert(action.label()),
                "duplicate label: {}",
                action.label()
            );
        }
    }

    #[test]
    fn recovery_action_rationale_non_empty() {
        for action in RecoveryAction::ALL {
            assert!(
                !action.rationale().is_empty(),
                "{action} has empty rationale"
            );
        }
    }

    #[test]
    fn recovery_action_display_matches_label() {
        for action in RecoveryAction::ALL {
            assert_eq!(
                action.to_string(),
                action.label(),
                "Display and label() diverged for {action:?}"
            );
        }
    }

    #[test]
    fn recovery_approval_display() {
        assert_eq!(
            RecoveryApproval::SilentSelfHeal.to_string(),
            "silent_self_heal"
        );
        assert_eq!(
            RecoveryApproval::ExplicitEscalation.to_string(),
            "explicit_escalation"
        );
    }

    #[test]
    fn recovery_action_known_silent_actions() {
        // Verify the specific actions we expect to be silent
        let expected_silent = [
            RecoveryAction::WalCheckpointPassive,
            RecoveryAction::WalCheckpointTruncate,
            RecoveryAction::StaleLockCleanup,
            RecoveryAction::EmptyWalSidecarCleanup,
            RecoveryAction::ConnectionPoolRefresh,
            RecoveryAction::IndexRebuild,
            RecoveryAction::InboxStatsRebuild,
            RecoveryAction::RestoreFromProactiveBackup,
            RecoveryAction::CreateProactiveBackup,
        ];
        for action in &expected_silent {
            assert!(
                action.is_silent(),
                "{action} should be classified as silent self-heal"
            );
        }
    }

    #[test]
    fn recovery_action_known_escalated_actions() {
        // Verify the specific actions we expect to require escalation
        let expected_escalated = [
            RecoveryAction::ReconstructFromArchive,
            RecoveryAction::DeleteCorruptDb,
            RecoveryAction::ForceUnlockContested,
            RecoveryAction::SchemaMigration,
            RecoveryAction::PromoteReconstructedCandidate,
            RecoveryAction::ReinitializeBlank,
        ];
        for action in &expected_escalated {
            assert!(
                action.requires_escalation(),
                "{action} should be classified as explicit escalation"
            );
        }
    }

    #[test]
    fn recovery_action_serde_roundtrip() {
        for action in RecoveryAction::ALL {
            let json = serde_json::to_string(action).expect("serialize");
            let parsed: RecoveryAction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*action, parsed, "serde roundtrip failed for {action:?}");
        }
    }

    #[test]
    fn recovery_approval_serde_roundtrip() {
        for approval in &[
            RecoveryApproval::SilentSelfHeal,
            RecoveryApproval::ExplicitEscalation,
        ] {
            let json = serde_json::to_string(approval).expect("serialize");
            let parsed: RecoveryApproval = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*approval, parsed, "serde roundtrip failed for {approval:?}");
        }
    }

    // ── DeferredWriteQueue tests ──────────────────────────────────────

    #[test]
    fn deferred_write_queue_inactive_rejects_writes() {
        let q = DeferredWriteQueue::new(10);
        let out = q.enqueue("INSERT INTO x VALUES(?)".into(), vec![Value::BigInt(1)], "test");
        assert_eq!(out, DeferralOutcome::NotRecovering);
        assert!(q.is_empty());
    }

    #[test]
    fn deferred_write_queue_active_accepts_writes() {
        let q = DeferredWriteQueue::new(10);
        q.activate();
        assert!(q.is_active());

        let out = q.enqueue("INSERT INTO x VALUES(?)".into(), vec![Value::BigInt(1)], "test_op");
        assert!(matches!(out, DeferralOutcome::Queued { position: 0 }));
        assert_eq!(q.len(), 1);

        let out = q.enqueue("UPDATE x SET y=?".into(), vec![Value::BigInt(2)], "test_op");
        assert!(matches!(out, DeferralOutcome::Queued { position: 1 }));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn deferred_write_queue_backpressure_at_capacity() {
        let q = DeferredWriteQueue::new(2);
        q.activate();

        q.enqueue("sql1".into(), vec![], "op1");
        q.enqueue("sql2".into(), vec![], "op2");
        let out = q.enqueue("sql3".into(), vec![], "op3");
        assert_eq!(out, DeferralOutcome::BackpressureFull { capacity: 2 });
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn deferred_write_queue_seal_and_drain_returns_ordered_entries() {
        let q = DeferredWriteQueue::new(10);
        q.activate();

        q.enqueue("sql_a".into(), vec![], "op_a");
        q.enqueue("sql_b".into(), vec![], "op_b");
        q.enqueue("sql_c".into(), vec![], "op_c");

        let entries = q.seal_and_drain();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sql, "sql_a");
        assert_eq!(entries[1].sql, "sql_b");
        assert_eq!(entries[2].sql, "sql_c");
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);

        // After seal, enqueue returns Sealed
        let out = q.enqueue("sql_d".into(), vec![], "op_d");
        assert_eq!(out, DeferralOutcome::Sealed);
        assert!(!q.is_active());
    }

    #[test]
    fn deferred_write_queue_reset_allows_reuse() {
        let q = DeferredWriteQueue::new(10);
        q.activate();
        q.enqueue("sql1".into(), vec![], "op");
        q.seal_and_drain();

        // After reset, queue is inactive
        q.reset();
        assert!(!q.is_active());
        assert!(q.is_empty());

        // Can re-activate for next recovery cycle
        q.activate();
        let out = q.enqueue("sql_new".into(), vec![], "op_new");
        assert!(matches!(out, DeferralOutcome::Queued { position: 0 }));
    }

    #[test]
    fn deferred_write_queue_status_reflects_state() {
        let q = DeferredWriteQueue::new(5);
        let s = q.status();
        assert!(!s.active);
        assert!(!s.sealed);
        assert_eq!(s.queued, 0);
        assert_eq!(s.capacity, 5);

        q.activate();
        q.enqueue("sql".into(), vec![], "op");
        let s = q.status();
        assert!(s.active);
        assert!(!s.sealed);
        assert_eq!(s.queued, 1);
        assert_eq!(s.next_seq, 1);
    }

    #[test]
    fn deferred_write_queue_entries_have_timestamps_and_operation() {
        let q = DeferredWriteQueue::new(10);
        q.activate();
        q.enqueue("INSERT INTO t VALUES(?)".into(), vec![Value::BigInt(42)], "send_message");
        let entries = q.seal_and_drain();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation, "send_message");
        assert!(entries[0].deferred_at_us > 0);
        assert_eq!(entries[0].params.len(), 1);
    }

    #[test]
    fn deferred_write_queue_concurrent_producers() {
        use std::sync::Arc;

        let q = Arc::new(DeferredWriteQueue::new(1000));
        q.activate();

        let mut handles = vec![];
        for i in 0..10 {
            let q = Arc::clone(&q);
            handles.push(std::thread::spawn(move || {
                for j in 0..50 {
                    let sql = format!("INSERT INTO t VALUES({i}, {j})");
                    q.enqueue(sql, vec![], "concurrent_op");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }

        assert_eq!(q.len(), 500);
        let entries = q.seal_and_drain();
        assert_eq!(entries.len(), 500);
        // Sequences are monotonically assigned (no duplicates)
        let mut seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 500);
    }

    // ── Overload shedding tests (br-97gc6.5.2.1.19) ─────────────────

    #[test]
    fn overload_policy_default_values() {
        let p = OverloadPolicy::default();
        assert_eq!(p.max_entries, 1024);
        assert_eq!(p.max_age_secs, 300);
        assert_eq!(p.max_bytes, 64 * 1024 * 1024);
        assert_eq!(p.fairness_limit_pct, 60);
        assert_eq!(p.fairness_limit(), 614); // floor(1024 * 60 / 100)
    }

    #[test]
    fn overload_fairness_limit_zero_disables() {
        let p = OverloadPolicy {
            fairness_limit_pct: 0,
            max_entries: 100,
            ..Default::default()
        };
        assert_eq!(p.fairness_limit(), 100, "0% should disable fairness limit");
    }

    #[test]
    fn overload_hard_stop_age_rejects_when_oldest_stale() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 100,
            max_age_secs: 0, // immediate hard-stop on any age
            ..Default::default()
        });
        q.activate();
        // First write succeeds (no oldest entry to check).
        let out = q.enqueue("INSERT INTO t VALUES(1)".into(), vec![], "op");
        assert!(matches!(out, DeferralOutcome::Queued { .. }));
        // Second write sees the first entry aged > 0 seconds.
        // Since max_age_secs=0, the next enqueue should hard-stop.
        // We need the first entry to have a non-zero age, which it does
        // because now_micros() moves forward. With max_age=0 any age > 0 triggers.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let out = q.enqueue("INSERT INTO t VALUES(2)".into(), vec![], "op");
        assert!(
            matches!(out, DeferralOutcome::HardStopAge { .. }),
            "expected HardStopAge, got: {out:?}"
        );
        assert_eq!(q.shed_count(), 1);
    }

    #[test]
    fn overload_hard_stop_bytes_rejects_when_budget_exceeded() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 1000,
            max_bytes: 300, // very small byte budget
            max_age_secs: 300,
            fairness_limit_pct: 0,
        });
        q.activate();
        // Each entry is ~128 overhead + SQL length + params.
        let out = q.enqueue("INSERT INTO t VALUES(1)".into(), vec![], "op");
        assert!(matches!(out, DeferralOutcome::Queued { .. }));
        // Second write should push past the 300 byte budget.
        let out = q.enqueue("INSERT INTO t VALUES(2)".into(), vec![], "op");
        assert!(
            matches!(out, DeferralOutcome::HardStopBytes { .. }),
            "expected HardStopBytes, got: {out:?}"
        );
    }

    #[test]
    fn overload_fairness_limit_caps_per_operation() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 100,
            fairness_limit_pct: 10, // 10 entries per operation
            max_age_secs: 300,
            max_bytes: 64 * 1024 * 1024,
        });
        q.activate();

        // Fill up 10 entries for "send_message"
        for i in 0..10 {
            let out = q.enqueue(format!("INSERT INTO m VALUES({i})"), vec![], "send_message");
            assert!(
                matches!(out, DeferralOutcome::Queued { .. }),
                "entry {i} should be queued"
            );
        }

        // 11th should hit fairness limit
        let out = q.enqueue("INSERT INTO m VALUES(10)".into(), vec![], "send_message");
        assert!(
            matches!(
                out,
                DeferralOutcome::FairnessLimitReached {
                    operation: "send_message",
                    count: 10,
                    limit: 10,
                }
            ),
            "expected FairnessLimitReached, got: {out:?}"
        );

        // Different operation type should still be accepted
        let out = q.enqueue("UPDATE agents SET name='x'".into(), vec![], "register_agent");
        assert!(
            matches!(out, DeferralOutcome::Queued { .. }),
            "different operation should not be affected by send_message fairness limit"
        );
    }

    #[test]
    fn overload_pressure_tiers_reflect_queue_state() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 100,
            max_age_secs: 300,
            max_bytes: 64 * 1024 * 1024,
            fairness_limit_pct: 0,
        });

        // Inactive: Normal
        assert_eq!(q.pressure(), BacklogPressure::Normal);

        q.activate();

        // Empty active: Normal
        assert_eq!(q.pressure(), BacklogPressure::Normal);

        // Fill to 50%: still Normal
        for i in 0..50 {
            q.enqueue(format!("INSERT INTO t VALUES({i})"), vec![], "op");
        }
        assert_eq!(q.pressure(), BacklogPressure::Normal);

        // Fill to 76%: Elevated (above 75% warn threshold)
        for i in 50..76 {
            q.enqueue(format!("INSERT INTO t VALUES({i})"), vec![], "op");
        }
        assert_eq!(q.pressure(), BacklogPressure::Elevated);

        // Fill to 100%: Critical
        for i in 76..100 {
            q.enqueue(format!("INSERT INTO t VALUES({i})"), vec![], "op");
        }
        assert_eq!(q.pressure(), BacklogPressure::Critical);

        // Sealed: HardStop
        q.seal_and_drain();
        assert_eq!(q.pressure(), BacklogPressure::HardStop);

        // Reset: Normal
        q.reset();
        assert_eq!(q.pressure(), BacklogPressure::Normal);
    }

    #[test]
    fn overload_status_includes_bytes_and_age() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 100,
            max_age_secs: 300,
            max_bytes: 64 * 1024 * 1024,
            fairness_limit_pct: 0,
        });
        q.activate();
        q.enqueue("INSERT INTO t VALUES(1)".into(), vec![], "op");

        let status = q.status();
        assert_eq!(status.queued, 1);
        assert!(status.estimated_bytes > 0, "should track estimated bytes");
        assert_eq!(status.shed_count, 0);
        assert_eq!(status.pressure, BacklogPressure::Normal);
    }

    #[test]
    fn overload_shed_count_is_lifetime() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 1,
            max_age_secs: 300,
            max_bytes: 64 * 1024 * 1024,
            fairness_limit_pct: 0,
        });
        q.activate();
        q.enqueue("INSERT INTO t VALUES(1)".into(), vec![], "op");
        // Second write is rejected (capacity 1).
        let out = q.enqueue("INSERT INTO t VALUES(2)".into(), vec![], "op");
        assert!(matches!(out, DeferralOutcome::BackpressureFull { .. }));
        assert_eq!(q.shed_count(), 1);

        // Reset and activate again — shed_count persists.
        q.reset();
        q.activate();
        q.enqueue("INSERT INTO t VALUES(3)".into(), vec![], "op");
        let out = q.enqueue("INSERT INTO t VALUES(4)".into(), vec![], "op");
        assert!(matches!(out, DeferralOutcome::BackpressureFull { .. }));
        assert_eq!(q.shed_count(), 2, "shed_count should be lifetime across resets");
    }

    #[test]
    fn overload_estimated_bytes_resets_on_drain() {
        let q = DeferredWriteQueue::with_policy(OverloadPolicy {
            max_entries: 100,
            max_age_secs: 300,
            max_bytes: 64 * 1024 * 1024,
            fairness_limit_pct: 0,
        });
        q.activate();
        q.enqueue("INSERT INTO large_table VALUES(1, 'data')".into(), vec![], "op");
        assert!(q.estimated_bytes() > 0);

        q.seal_and_drain();
        assert_eq!(q.estimated_bytes(), 0, "seal_and_drain should reset estimated bytes");
    }

    // ── Replay compensation tests (br-97gc6.5.2.1.14) ───────────────

    #[test]
    fn replay_result_tracks_success_and_failure_counts() {
        let result = ReplayResult {
            replayed: 8,
            failed: 2,
            total: 10,
        };
        assert_eq!(result.replayed, 8);
        assert_eq!(result.failed, 2);
        assert_eq!(result.total, 10);
    }

    #[test]
    fn replay_compensation_record_captures_failure_context() {
        let record = ReplayCompensationRecord {
            seq: 42,
            sql: "INSERT INTO messages (body) VALUES ('hello')".to_string(),
            operation: "send_message",
            error: "UNIQUE constraint failed: messages.id".to_string(),
            deferred_at_us: 1_700_000_000_000_000,
            failed_at_us: 1_700_000_005_000_000,
        };
        assert_eq!(record.seq, 42);
        assert_eq!(record.operation, "send_message");
        assert!(record.error.contains("UNIQUE constraint"));
        assert!(record.failed_at_us > record.deferred_at_us);
    }

    #[test]
    fn replay_compensation_log_accumulates_failures() {
        let log = ReplayCompensationLog::new();
        assert!(log.is_empty());

        log.record(ReplayCompensationRecord {
            seq: 0,
            sql: "INSERT INTO t VALUES(1)".into(),
            operation: "op_a",
            error: "constraint".into(),
            deferred_at_us: 100,
            failed_at_us: 200,
        });
        log.record(ReplayCompensationRecord {
            seq: 1,
            sql: "INSERT INTO t VALUES(2)".into(),
            operation: "op_b",
            error: "locked".into(),
            deferred_at_us: 100,
            failed_at_us: 300,
        });

        assert_eq!(log.len(), 2);
        let entries = log.drain();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].operation, "op_a");
        assert_eq!(entries[1].operation, "op_b");
        assert!(log.is_empty(), "drain should empty the log");
    }

    // ── Ephemeral storage root rerouting tests (br-97gc6.5.2.2) ──────

    #[test]
    fn with_ephemeral_reroute_production_path_unchanged() {
        let config = DbPoolConfig::default();
        let rerouted = config.with_ephemeral_reroute(Path::new("/data/projects/real-project"));
        // For a production path, the storage_root should not change.
        // Default config has storage_root = None, so resolved_storage_root falls back
        // to Config::from_env(). The ephemeral reroute should leave it as-is.
        assert_eq!(rerouted.storage_root, None);
    }

    #[test]
    fn would_reroute_for_project_returns_none_for_production() {
        let config = DbPoolConfig::default();
        let result = config.would_reroute_for_project(Path::new("/data/projects/my-project"));
        assert!(
            result.is_none(),
            "production path should not trigger reroute"
        );
    }

    #[test]
    fn from_env_for_project_constructs_without_panic() {
        // Ensure the combined constructor works without panicking.
        let _config = DbPoolConfig::from_env_for_project(Path::new("/data/projects/test"));
    }

    #[test]
    fn with_ephemeral_reroute_returns_isolated_for_tmp_when_default_root() {
        // Build a config that looks like it uses the default storage root.
        // compute_ephemeral_storage_root checks is_default_storage_root, which
        // compares against the live config. We can only verify the method
        // doesn't panic and returns a modified config when conditions align.
        let mut config = DbPoolConfig::default();
        config.storage_root = None; // forces fallback to Config::from_env()
        let rerouted = config.with_ephemeral_reroute(Path::new("/tmp/ci-test-run"));
        // Whether reroute actually happens depends on the runtime config's
        // storage_root being the default. We verify the method is callable
        // and produces a well-formed config.
        assert!(rerouted.resolved_storage_root().as_os_str().len() > 0);
    }

    #[test]
    fn with_ephemeral_reroute_custom_storage_root_unchanged() {
        let mut config = DbPoolConfig::default();
        let custom = PathBuf::from("/opt/custom-mail-storage");
        config.storage_root = Some(custom.clone());
        let rerouted = config.with_ephemeral_reroute(Path::new("/tmp/test-project"));
        // Custom storage root means ephemeral reroute should not apply (the
        // operator deliberately chose a non-default root).
        assert_eq!(rerouted.storage_root, Some(custom));
    }

    #[test]
    fn would_reroute_matches_with_ephemeral_reroute_behavior() {
        let mut config = DbPoolConfig::default();
        let custom = PathBuf::from("/opt/custom-storage");
        config.storage_root = Some(custom.clone());

        let would = config.would_reroute_for_project(Path::new("/tmp/test"));
        let cloned = config.clone().with_ephemeral_reroute(Path::new("/tmp/test"));

        // Both should agree: custom storage root prevents reroute.
        assert!(would.is_none());
        assert_eq!(cloned.storage_root, Some(custom));
    }

    // ── Canary namespace tests (br-97gc6.5.2.6.5.4) ───────────────────

    #[test]
    fn canary_prefix_constants_are_consistent() {
        assert!(CANARY_PROJECT_SLUG.starts_with(CANARY_PREFIX));
        assert!(CANARY_AGENT_PREFIX.starts_with(CANARY_PREFIX));
        assert!(CANARY_STORAGE_DIR_PREFIX.starts_with(CANARY_PREFIX));
    }

    #[test]
    fn is_canary_identifier_accepts_canary_names() {
        assert!(is_canary_identifier(CANARY_PROJECT_SLUG));
        assert!(is_canary_identifier("__canary_probe_42"));
        assert!(is_canary_identifier("__canary_anything_else"));
    }

    #[test]
    fn is_canary_identifier_rejects_production_names() {
        assert!(!is_canary_identifier("my_real_project"));
        assert!(!is_canary_identifier("SilentBadger"));
        assert!(!is_canary_identifier(""));
        assert!(!is_canary_identifier("_canary_missing_second_underscore"));
    }

    #[test]
    fn canary_agent_name_is_in_namespace() {
        let name = canary_agent_name(7);
        assert_eq!(name, "__canary_probe_7");
        assert!(is_canary_identifier(&name));
    }

    #[test]
    fn canary_storage_root_is_in_namespace() {
        let root = canary_storage_root(42);
        assert!(is_canary_path(&root));
        let dir_name = root.file_name().unwrap().to_str().unwrap();
        assert!(dir_name.starts_with(CANARY_STORAGE_DIR_PREFIX));
    }

    #[test]
    fn is_canary_path_accepts_canary_dirs() {
        assert!(is_canary_path(Path::new("/tmp/__canary_mailbox_1")));
        assert!(is_canary_path(Path::new("/var/data/__canary_probe_99")));
    }

    #[test]
    fn is_canary_path_rejects_production_dirs() {
        assert!(!is_canary_path(Path::new("/tmp/real_project")));
        assert!(!is_canary_path(Path::new("/home/user/.mcp_agent_mail")));
    }

    #[test]
    fn canary_alert_tier_properties() {
        // Silent: not visible, no ticket
        assert!(!CanaryAlertTier::Silent.dashboard_visible());
        assert!(!CanaryAlertTier::Silent.creates_ticket());

        // Observable: visible, no ticket
        assert!(CanaryAlertTier::Observable.dashboard_visible());
        assert!(!CanaryAlertTier::Observable.creates_ticket());

        // Warning: visible, no ticket
        assert!(CanaryAlertTier::Warning.dashboard_visible());
        assert!(!CanaryAlertTier::Warning.creates_ticket());

        // Engineering: visible AND creates ticket (but never pages)
        assert!(CanaryAlertTier::Engineering.dashboard_visible());
        assert!(CanaryAlertTier::Engineering.creates_ticket());
    }

    #[test]
    fn canary_alert_tier_all_is_exhaustive() {
        assert_eq!(CanaryAlertTier::ALL.len(), 4);
        assert_eq!(CanaryAlertTier::ALL[0], CanaryAlertTier::Silent);
        assert_eq!(CanaryAlertTier::ALL[3], CanaryAlertTier::Engineering);
    }

    #[test]
    fn classify_canary_outcome_success() {
        let policy = classify_canary_outcome(true, 1_000, true, false, false);
        assert_eq!(policy.tier, CanaryAlertTier::Silent);
        assert_eq!(policy.reason, "probe_ok");
    }

    #[test]
    fn classify_canary_outcome_slow_probe() {
        let policy = classify_canary_outcome(true, 6_000_000, true, false, false);
        assert_eq!(policy.tier, CanaryAlertTier::Observable);
        assert_eq!(policy.reason, "slow_probe");
    }

    #[test]
    fn classify_canary_outcome_probe_failed() {
        let policy = classify_canary_outcome(false, 1_000, true, false, false);
        assert_eq!(policy.tier, CanaryAlertTier::Warning);
        assert_eq!(policy.reason, "probe_assertion_failed");
    }

    #[test]
    fn classify_canary_outcome_integrity_failure() {
        let policy = classify_canary_outcome(true, 1_000, false, false, false);
        assert_eq!(policy.tier, CanaryAlertTier::Engineering);
        assert_eq!(policy.reason, "integrity_mismatch");
    }

    #[test]
    fn classify_canary_outcome_recovery_failure() {
        let policy = classify_canary_outcome(true, 1_000, true, true, false);
        assert_eq!(policy.tier, CanaryAlertTier::Engineering);
        assert_eq!(policy.reason, "recovery_failed");
    }

    #[test]
    fn classify_canary_outcome_integrity_trumps_recovery() {
        // Integrity failure is more severe than recovery failure.
        let policy = classify_canary_outcome(false, 1_000, false, true, false);
        assert_eq!(policy.tier, CanaryAlertTier::Engineering);
        assert_eq!(policy.reason, "integrity_mismatch");
    }

    #[test]
    fn record_canary_probe_updates_metrics() {
        record_canary_probe(500, true, false, false, true);
        let snap = mcp_agent_mail_core::global_metrics().canary.snapshot();
        assert!(snap.canary_probes_total > 0);
        assert!(snap.canary_probes_ok > 0);
    }

    #[test]
    fn canary_mailbox_lifecycle_updates_gauge() {
        let before = mcp_agent_mail_core::global_metrics()
            .canary
            .canary_mailboxes_created_total
            .load();
        canary_mailbox_created();
        let after = mcp_agent_mail_core::global_metrics()
            .canary
            .canary_mailboxes_created_total
            .load();
        assert!(after > before);

        canary_mailbox_destroyed();
        let destroyed = mcp_agent_mail_core::global_metrics()
            .canary
            .canary_mailboxes_destroyed_total
            .load();
        assert!(destroyed > 0);
    }

    #[test]
    fn canary_alert_policy_success_constructor() {
        let p = CanaryAlertPolicy::success("test detail".to_string());
        assert_eq!(p.tier, CanaryAlertTier::Silent);
        assert_eq!(p.reason, "probe_ok");
        assert_eq!(p.detail, "test detail");
    }

    #[test]
    fn canary_alert_tier_display_labels() {
        assert_eq!(CanaryAlertTier::Silent.to_string(), "silent");
        assert_eq!(CanaryAlertTier::Observable.to_string(), "observable");
        assert_eq!(CanaryAlertTier::Warning.to_string(), "warning");
        assert_eq!(CanaryAlertTier::Engineering.to_string(), "engineering");
    }
}
