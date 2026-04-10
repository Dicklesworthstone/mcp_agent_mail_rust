//! Centralized mailbox durability verdict computation.
//!
//! This module provides the authoritative health-verdict engine consumed by
//! startup, doctor, CLI, and server code. It implements the state machine
//! defined in `docs/SPEC-mailbox-durability-states.md`.
//!
//! # States
//!
//! - `Healthy`: All probes pass, ready for reads and writes
//! - `Stale`: DB accessible but may not reflect latest archive state
//! - `Suspect`: Anomalies detected but not definitively broken
//! - `Broken`: Definitively corrupted or inaccessible
//! - `Recovering`: Exclusive recovery operation in progress
//! - `DegradedReadOnly`: Safe for reads but writes blocked
//! - `Escalate`: Requires human intervention
//!
//! # Usage
//!
//! ```ignore
//! use mcp_agent_mail_db::mailbox_verdict::{compute_mailbox_verdict, VerdictOptions};
//!
//! let verdict = compute_mailbox_verdict(database_url, archive_root, &VerdictOptions::default());
//! if verdict.state.allows_writes() {
//!     // Safe to proceed with mutations
//! }
//! ```

use crate::integrity::{
    CheckKind, MailboxIntegrityStatus, MailboxIntegrityVerdict, inspect_mailbox_integrity,
};
use crate::pool::{
    MailboxOwnershipDisposition, MailboxOwnershipState, MailboxRecoveryLockState,
    MailboxSidecarState, inspect_mailbox_db_inventory, inspect_mailbox_ownership,
    inspect_mailbox_recovery_lock, inspect_mailbox_sidecar_state, resolve_mailbox_sqlite_path,
    sqlite_compatibility_read_path_is_healthy, sqlite_primary_read_path_is_healthy,
};
use crate::reconstruct::{archive_missing_project_identities, scan_archive_message_inventory};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ============================================================================
// State enumeration
// ============================================================================

/// Canonical mailbox durability states.
///
/// These states form a severity hierarchy:
/// `Escalate > Broken > Recovering > Suspect > Stale > DegradedReadOnly > Healthy`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxState {
    /// All probes pass, archive/DB in sync, no anomalies.
    Healthy,
    /// DB accessible but may not reflect latest archive state.
    Stale,
    /// Anomalies detected but not definitively broken.
    Suspect,
    /// Definitively corrupted or inaccessible.
    Broken,
    /// Exclusive recovery operation in progress.
    Recovering,
    /// Safe for reads but writes blocked pending repair.
    DegradedReadOnly,
    /// Requires human intervention; automated recovery unsafe.
    Escalate,
}

impl MailboxState {
    /// Whether read operations are allowed in this state.
    #[must_use]
    pub const fn allows_reads(&self) -> bool {
        matches!(
            self,
            Self::Healthy | Self::Stale | Self::Suspect | Self::DegradedReadOnly
        )
    }

    /// Whether write operations are allowed in this state.
    #[must_use]
    pub const fn allows_writes(&self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Whether recovery operations are allowed to start in this state.
    #[must_use]
    pub const fn allows_recovery_start(&self) -> bool {
        matches!(self, Self::Broken | Self::DegradedReadOnly | Self::Suspect)
    }

    /// Severity level (higher = worse). Used for composite evidence.
    #[must_use]
    pub const fn severity(&self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::DegradedReadOnly => 1,
            Self::Stale => 2,
            Self::Suspect => 3,
            Self::Recovering => 4,
            Self::Broken => 5,
            Self::Escalate => 6,
        }
    }

    /// Returns the more severe of two states.
    #[must_use]
    pub fn max_severity(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    /// Human-readable status string for CLI/JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Stale => "stale",
            Self::Suspect => "suspect",
            Self::Broken => "broken",
            Self::Recovering => "recovering",
            Self::DegradedReadOnly => "degraded_read_only",
            Self::Escalate => "escalate",
        }
    }
}

impl std::fmt::Display for MailboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Runtime durability state (4-state operational machine)
// ============================================================================

/// Runtime durability state for the mailbox pool.
///
/// This is the coarse operational state that the DB pool tracks at runtime to
/// gate read/write admission. It collapses the 7-state diagnostic
/// [`MailboxState`] into four actionable buckets:
///
/// | `DurabilityState`   | Reads | Writes | Recovery |
/// |---------------------|-------|--------|----------|
/// | `Healthy`           | yes   | yes    | no       |
/// | `DegradedReadOnly`  | yes   | no     | optional |
/// | `Recovering`        | snapshot only | supervisor only | active |
/// | `Corrupt`           | no    | no     | required |
///
/// # Transition diagram
///
/// ```text
/// Healthy
///   ├─ drift / suspicion ──────────► DegradedReadOnly
///   └─ decisive failure ───────────► Corrupt
///
/// DegradedReadOnly
///   ├─ verified promotion ─────────► Healthy
///   ├─ supervisor starts repair ───► Recovering
///   └─ read path also fails ───────► Corrupt
///
/// Recovering
///   ├─ candidate promoted ─────────► Healthy
///   ├─ candidate failed, reads ok ─► DegradedReadOnly
///   └─ recovery aborted, no reads ─► Corrupt
///
/// Corrupt
///   └─ recovery attempt started ───► Recovering
/// ```
///
/// # Ownership invariants
///
/// 1. The verdict engine owns transitions into/out of `Healthy` and
///    `DegradedReadOnly`.
/// 2. The mailbox supervisor owns transitions into/out of `Recovering`.
/// 3. `Corrupt` requires operator or supervisor authorization to exit.
/// 4. Only one recovery owner may be active at a time (single-flight).
/// 5. Normal writes are blocked in every state except `Healthy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityState {
    /// All probes pass; reads and writes proceed normally.
    Healthy,
    /// Reads may continue from verified snapshots; writes are blocked.
    DegradedReadOnly,
    /// An exclusive recovery operation is in progress.
    Recovering,
    /// No safe read or write path; recovery is mandatory.
    Corrupt,
}

/// A single allowed runtime durability transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityTransition {
    /// Source state.
    pub from: DurabilityState,
    /// Destination state.
    pub to: DurabilityState,
    /// Short trigger description.
    pub trigger: &'static str,
}

/// All allowed transitions between runtime durability states.
pub const DURABILITY_TRANSITIONS: &[DurabilityTransition] = &[
    // Healthy exits
    DurabilityTransition {
        from: DurabilityState::Healthy,
        to: DurabilityState::DegradedReadOnly,
        trigger: "drift_or_suspicion_detected",
    },
    DurabilityTransition {
        from: DurabilityState::Healthy,
        to: DurabilityState::Corrupt,
        trigger: "decisive_failure",
    },
    // DegradedReadOnly exits
    DurabilityTransition {
        from: DurabilityState::DegradedReadOnly,
        to: DurabilityState::Healthy,
        trigger: "verified_promotion",
    },
    DurabilityTransition {
        from: DurabilityState::DegradedReadOnly,
        to: DurabilityState::Recovering,
        trigger: "supervisor_started_repair",
    },
    DurabilityTransition {
        from: DurabilityState::DegradedReadOnly,
        to: DurabilityState::Corrupt,
        trigger: "read_path_failed",
    },
    // Recovering exits
    DurabilityTransition {
        from: DurabilityState::Recovering,
        to: DurabilityState::Healthy,
        trigger: "candidate_promoted",
    },
    DurabilityTransition {
        from: DurabilityState::Recovering,
        to: DurabilityState::DegradedReadOnly,
        trigger: "candidate_failed_snapshots_remain",
    },
    DurabilityTransition {
        from: DurabilityState::Recovering,
        to: DurabilityState::Corrupt,
        trigger: "recovery_aborted_no_reads",
    },
    // Corrupt exits
    DurabilityTransition {
        from: DurabilityState::Corrupt,
        to: DurabilityState::Recovering,
        trigger: "recovery_attempt_started",
    },
];

impl DurabilityState {
    /// Whether read operations are permitted in this state.
    #[must_use]
    pub const fn allows_reads(self) -> bool {
        matches!(self, Self::Healthy | Self::DegradedReadOnly)
    }

    /// Whether normal (non-supervisor) write operations are permitted.
    #[must_use]
    pub const fn allows_writes(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Whether an exclusive recovery operation may be started from this state.
    #[must_use]
    pub const fn allows_recovery_start(self) -> bool {
        matches!(self, Self::DegradedReadOnly | Self::Corrupt)
    }

    /// Whether this state is degraded relative to normal operation.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        !matches!(self, Self::Healthy)
    }

    /// Severity ordinal (higher = worse).
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::DegradedReadOnly => 1,
            Self::Recovering => 2,
            Self::Corrupt => 3,
        }
    }

    /// Human-readable status string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::DegradedReadOnly => "degraded_read_only",
            Self::Recovering => "recovering",
            Self::Corrupt => "corrupt",
        }
    }

    /// Collapse the diagnostic 7-state [`MailboxState`] into the operational
    /// 4-state runtime representation.
    #[must_use]
    pub const fn from_mailbox_state(state: MailboxState) -> Self {
        match state {
            MailboxState::Healthy => Self::Healthy,
            MailboxState::Stale | MailboxState::Suspect | MailboxState::DegradedReadOnly => {
                Self::DegradedReadOnly
            }
            MailboxState::Recovering => Self::Recovering,
            MailboxState::Broken | MailboxState::Escalate => Self::Corrupt,
        }
    }

    /// Map from the backpressure [`HealthLevel`] to an operational floor.
    ///
    /// `HealthLevel` tracks system load (pool contention, queue depth), not
    /// data integrity. A `Red` health level alone does not imply corruption,
    /// but it may warrant degrading to read-only to shed write load.
    ///
    /// Returns the *minimum* durability state implied by the health level
    /// alone — callers should combine it with probe evidence via
    /// [`DurabilityState::max_severity`].
    #[must_use]
    pub const fn floor_from_health_level(level: mcp_agent_mail_core::HealthLevel) -> Self {
        match level {
            mcp_agent_mail_core::HealthLevel::Green | mcp_agent_mail_core::HealthLevel::Yellow => {
                Self::Healthy
            }
            mcp_agent_mail_core::HealthLevel::Red => Self::DegradedReadOnly,
        }
    }

    /// Returns the more severe of two states.
    #[must_use]
    pub const fn max_severity(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }
}

impl std::fmt::Display for DurabilityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validate a runtime durability transition.
///
/// Self-transitions are always allowed (idempotent). Returns the trigger
/// name on success, or an error message if the transition is not in
/// [`DURABILITY_TRANSITIONS`].
pub fn validate_durability_transition(
    from: DurabilityState,
    to: DurabilityState,
) -> Result<&'static str, &'static str> {
    if from == to {
        return Ok("idempotent");
    }
    DURABILITY_TRANSITIONS
        .iter()
        .find(|t| t.from == from && t.to == to)
        .map(|t| t.trigger)
        .ok_or("invalid durability state transition")
}

// ============================================================================
// Probe result types
// ============================================================================

/// Severity of a single probe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeSeverity {
    /// Informational, does not affect state.
    Info,
    /// Warning, may trigger `Suspect` or `Stale`.
    Warning,
    /// Error, triggers `Broken` or worse.
    Error,
    /// Fatal, triggers `Escalate`.
    Fatal,
}

/// Result of a single diagnostic probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Probe identifier.
    pub name: String,
    /// Whether the probe passed.
    pub passed: bool,
    /// Human-readable detail.
    pub detail: String,
    /// Severity level.
    pub severity: ProbeSeverity,
    /// Explicit durability-state impact for this probe.
    pub impact_state: MailboxState,
    /// Duration of the probe in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_us: Option<u64>,
}

impl ProbeResult {
    /// Create a passing probe result.
    #[must_use]
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            detail: detail.into(),
            severity: ProbeSeverity::Info,
            impact_state: MailboxState::Healthy,
            duration_us: None,
        }
    }

    /// Create a warning probe result.
    #[must_use]
    pub fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::warn_state(name, detail, MailboxState::Suspect)
    }

    /// Create a warning probe result with an explicit durability-state impact.
    #[must_use]
    pub fn warn_state(
        name: impl Into<String>,
        detail: impl Into<String>,
        impact_state: MailboxState,
    ) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail: detail.into(),
            severity: ProbeSeverity::Warning,
            impact_state,
            duration_us: None,
        }
    }

    /// Create an error probe result.
    #[must_use]
    pub fn error(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::error_state(name, detail, MailboxState::Broken)
    }

    /// Create an error probe result with an explicit durability-state impact.
    #[must_use]
    pub fn error_state(
        name: impl Into<String>,
        detail: impl Into<String>,
        impact_state: MailboxState,
    ) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail: detail.into(),
            severity: ProbeSeverity::Error,
            impact_state,
            duration_us: None,
        }
    }

    /// Create a fatal probe result.
    #[must_use]
    pub fn fatal(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail: detail.into(),
            severity: ProbeSeverity::Fatal,
            impact_state: MailboxState::Escalate,
            duration_us: None,
        }
    }

    /// Set the duration.
    #[must_use]
    pub fn with_duration(mut self, us: u64) -> Self {
        self.duration_us = Some(us);
        self
    }
}

// ============================================================================
// Verdict and evidence
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxSqlitePathVerdict {
    pub database_url: String,
    pub configured_path: Option<String>,
    pub canonical_path: Option<String>,
    pub used_absolute_fallback: bool,
    pub detail: String,
}

impl MailboxSqlitePathVerdict {
    fn from_resolution(database_url: &str) -> Result<Self, ProbeResult> {
        match resolve_mailbox_sqlite_path(database_url) {
            Ok(resolution) => Ok(Self {
                database_url: database_url.to_string(),
                configured_path: Some(resolution.configured_path.clone()),
                canonical_path: Some(resolution.canonical_path.clone()),
                used_absolute_fallback: resolution.used_absolute_fallback,
                detail: if resolution.used_absolute_fallback {
                    format!(
                        "Resolved malformed relative sqlite path {} to canonical {}",
                        resolution.configured_path, resolution.canonical_path
                    )
                } else {
                    format!(
                        "Canonical sqlite path resolved to {}",
                        resolution.canonical_path
                    )
                },
            }),
            Err(error) => Err(ProbeResult::error(
                "db_path_resolution",
                format!("Cannot resolve canonical sqlite path from DATABASE_URL: {error}"),
            )),
        }
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.canonical_path.as_ref().map(PathBuf::from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxArchiveDriftState {
    Skipped,
    Unknown,
    Aligned,
    ArchiveAhead,
    DbAhead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxArchiveDriftVerdict {
    pub state: MailboxArchiveDriftState,
    pub archive_projects: usize,
    pub archive_agents: usize,
    pub archive_messages: usize,
    pub db_projects: usize,
    pub db_agents: usize,
    pub db_messages: usize,
    pub archive_latest_message_id: Option<i64>,
    pub db_max_message_id: i64,
    pub missing_projects: Vec<String>,
    pub detail: String,
}

impl MailboxArchiveDriftVerdict {
    fn skipped(detail: impl Into<String>) -> Self {
        Self {
            state: MailboxArchiveDriftState::Skipped,
            archive_projects: 0,
            archive_agents: 0,
            archive_messages: 0,
            db_projects: 0,
            db_agents: 0,
            db_messages: 0,
            archive_latest_message_id: None,
            db_max_message_id: 0,
            missing_projects: Vec::new(),
            detail: detail.into(),
        }
    }
}

/// Complete mailbox health verdict with evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxHealthVerdict {
    /// The computed durability state.
    pub state: MailboxState,
    /// Canonicalized sqlite path resolution used to compute this verdict.
    pub sqlite_path: MailboxSqlitePathVerdict,
    /// Most recent live integrity assessment.
    pub integrity: MailboxIntegrityVerdict,
    /// Live WAL/SHM sidecar state.
    pub wal: MailboxSidecarState,
    /// Recovery lock state for the mailbox.
    pub recovery_lock: MailboxRecoveryLockState,
    /// Ownership/process liveness state for the mailbox.
    pub ownership: MailboxOwnershipState,
    /// Archive-vs-DB drift assessment.
    pub archive_drift: MailboxArchiveDriftVerdict,
    /// All probe results that led to this verdict.
    pub probes: Vec<ProbeResult>,
    /// Timestamp when verdict was computed (microseconds since epoch).
    pub timestamp: i64,
}

impl MailboxHealthVerdict {
    /// Create a verdict from probe results.
    #[must_use]
    pub fn from_components(
        sqlite_path: MailboxSqlitePathVerdict,
        integrity: MailboxIntegrityVerdict,
        wal: MailboxSidecarState,
        recovery_lock: MailboxRecoveryLockState,
        ownership: MailboxOwnershipState,
        archive_drift: MailboxArchiveDriftVerdict,
        probes: Vec<ProbeResult>,
    ) -> Self {
        let state = compute_state_from_probes(&probes, recovery_lock.active);
        let timestamp = mcp_agent_mail_core::timestamps::now_micros();
        Self {
            state,
            sqlite_path,
            integrity,
            wal,
            recovery_lock,
            ownership,
            archive_drift,
            probes,
            timestamp,
        }
    }

    /// Count of failing probes.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.probes.iter().filter(|p| !p.passed).count()
    }

    /// Count of warning-severity failures.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.probes
            .iter()
            .filter(|p| !p.passed && p.severity == ProbeSeverity::Warning)
            .count()
    }

    /// Count of error-severity failures.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.probes
            .iter()
            .filter(|p| !p.passed && p.severity == ProbeSeverity::Error)
            .count()
    }
}

/// Whether read surfaces should prefer archive snapshots over the live DB.
///
/// Most degraded verdicts mean the live mailbox is unsafe or untrustworthy for
/// direct reads. The exception is archive parity drift where the DB is ahead of
/// the archive: that signals delayed archive persistence, not a reason to hide
/// newer live data behind an older archive snapshot.
#[must_use]
pub fn verdict_prefers_archive_snapshot_reads(verdict: &MailboxHealthVerdict) -> bool {
    let durability = DurabilityState::from_mailbox_state(verdict.state);
    if !durability.allows_reads() {
        return true;
    }
    if !durability.is_degraded() {
        return false;
    }

    let only_archive_parity_failures = verdict
        .probes
        .iter()
        .filter(|probe| !probe.passed)
        .all(|probe| probe.name == "archive_db_parity");
    if only_archive_parity_failures
        && verdict.archive_drift.state == MailboxArchiveDriftState::DbAhead
    {
        return false;
    }

    true
}

/// Whether read surfaces using the primary FrankenSQLite path should prefer
/// archive snapshots over the live DB.
///
/// The full mailbox verdict includes a secondary compatibility reopen/probe.
/// That probe is useful for diagnostics, but it should not force archive
/// fallback when the primary read path is still healthy, because the read
/// surfaces themselves execute on the primary path.
#[must_use]
pub fn verdict_prefers_archive_snapshot_reads_for_primary_read_surface(
    verdict: &MailboxHealthVerdict,
    sqlite_path: &Path,
) -> bool {
    if !verdict_prefers_archive_snapshot_reads(verdict) {
        return false;
    }

    let only_db_sanity_failures = verdict
        .probes
        .iter()
        .filter(|probe| !probe.passed)
        .all(|probe| probe.name == "db_sanity");
    if only_db_sanity_failures
        && crate::pool::sqlite_primary_read_path_is_healthy(sqlite_path).unwrap_or(false)
    {
        return false;
    }

    true
}

/// Compute the state from probe results.
fn compute_state_from_probes(probes: &[ProbeResult], recovery_lock_held: bool) -> MailboxState {
    let mut state = MailboxState::Healthy;

    for probe in probes {
        if probe.passed {
            continue;
        }

        state = state.max_severity(probe.impact_state);
    }

    if state == MailboxState::Escalate {
        return state;
    }

    if recovery_lock_held {
        MailboxState::Recovering
    } else {
        state
    }
}

// ============================================================================
// Verdict options
// ============================================================================

/// Options for verdict computation.
#[derive(Debug, Clone)]
pub struct VerdictOptions {
    /// Skip archive count check (for offline/fast-path).
    pub skip_archive_count: bool,
    /// Stale threshold: percentage mismatch.
    pub stale_threshold_pct: f64,
    /// Stale threshold: absolute message count.
    pub stale_threshold_abs: usize,
    /// Skip integrity check (expensive).
    pub skip_integrity_check: bool,
    /// Check for recovery lock.
    pub check_recovery_lock: bool,
}

impl Default for VerdictOptions {
    fn default() -> Self {
        Self {
            skip_archive_count: false,
            stale_threshold_pct: 0.05,
            stale_threshold_abs: 100,
            skip_integrity_check: false,
            check_recovery_lock: true,
        }
    }
}

impl VerdictOptions {
    /// Fast options suitable for hot-path checks.
    #[must_use]
    pub fn fast() -> Self {
        Self {
            skip_archive_count: true,
            skip_integrity_check: true,
            check_recovery_lock: true,
            ..Default::default()
        }
    }
}

// ============================================================================
// Core verdict computation
// ============================================================================

/// Compute the mailbox health verdict.
///
/// This is the canonical entry point for all health checks. It runs layered
/// probes and returns a complete verdict with evidence.
///
/// # Arguments
///
/// * `database_url` - Raw `DATABASE_URL` input to canonicalize before probing
/// * `archive_root` - Path to the Git archive root directory
/// * `options` - Options controlling which probes to run
///
/// # Returns
///
/// A `MailboxHealthVerdict` containing the state and all probe results.
#[must_use]
pub fn compute_mailbox_verdict(
    database_url: &str,
    archive_root: &Path,
    options: &VerdictOptions,
) -> MailboxHealthVerdict {
    let mut probes = Vec::new();
    let sqlite_path = match MailboxSqlitePathVerdict::from_resolution(database_url) {
        Ok(sqlite_path) => sqlite_path,
        Err(probe) => {
            let sqlite_path = MailboxSqlitePathVerdict {
                database_url: database_url.to_string(),
                configured_path: None,
                canonical_path: None,
                used_absolute_fallback: false,
                detail: probe.detail.clone(),
            };
            probes.push(probe);
            return MailboxHealthVerdict::from_components(
                sqlite_path,
                MailboxIntegrityVerdict {
                    status: MailboxIntegrityStatus::Skipped,
                    metrics: crate::integrity::integrity_metrics(),
                    check: None,
                    detail: "Integrity check skipped because canonical path resolution failed"
                        .to_string(),
                },
                MailboxSidecarState::default(),
                MailboxRecoveryLockState {
                    lock_path: String::new(),
                    exists: false,
                    active: false,
                    pid: None,
                    detail:
                        "Recovery lock inspection skipped because canonical path resolution failed"
                            .to_string(),
                },
                MailboxOwnershipState {
                    disposition: MailboxOwnershipDisposition::Unowned,
                    storage_lock_path: archive_root
                        .join(".mailbox.activity.lock")
                        .display()
                        .to_string(),
                    sqlite_lock_path: String::new(),
                    processes: Vec::new(),
                    competing_pids: Vec::new(),
                    supervised_restart_required: false,
                    detail: "Ownership inspection skipped because canonical path resolution failed"
                        .to_string(),
                },
                MailboxArchiveDriftVerdict::skipped(
                    "Archive drift inspection skipped because canonical path resolution failed",
                ),
                probes,
            );
        }
    };
    let db_path = sqlite_path
        .db_path()
        .unwrap_or_else(|| PathBuf::from(":memory:"));

    probes.push(probe_db_file_exists(&db_path));
    if probes.last().is_some_and(|probe| probe.passed) {
        probes.push(probe_db_file_sanity(&db_path));
    }

    probes.push(probe_archive_accessible(archive_root));
    probes.push(probe_archive_writable(archive_root));

    let wal = inspect_mailbox_sidecar_state(&db_path);
    probes.push(probe_sidecar_state(&wal));

    let recovery_lock = if options.check_recovery_lock {
        inspect_mailbox_recovery_lock(&db_path)
    } else {
        MailboxRecoveryLockState {
            lock_path: format!("{}.recovery.lock", db_path.display()),
            exists: false,
            active: false,
            pid: None,
            detail: "Recovery lock inspection disabled by options".to_string(),
        }
    };
    if options.check_recovery_lock {
        probes.push(probe_recovery_lock(&recovery_lock));
    }

    let ownership = inspect_mailbox_ownership(&db_path, archive_root);
    probes.push(probe_mailbox_ownership(&ownership));

    let integrity = if !options.skip_integrity_check
        && probes
            .iter()
            .all(|probe| probe.passed || probe.severity == ProbeSeverity::Warning)
    {
        inspect_mailbox_integrity(&db_path, CheckKind::Quick)
    } else {
        MailboxIntegrityVerdict {
            status: MailboxIntegrityStatus::Skipped,
            metrics: crate::integrity::integrity_metrics(),
            check: None,
            detail: "Integrity check skipped by verdict options or earlier decisive failures"
                .to_string(),
        }
    };
    probes.push(probe_integrity(&integrity));

    let archive_drift = if options.skip_archive_count {
        MailboxArchiveDriftVerdict::skipped("Archive drift inspection disabled by options")
    } else {
        inspect_archive_drift(&db_path, archive_root)
    };
    probes.push(probe_schema_populated(
        &db_path,
        archive_drift.archive_messages,
    ));
    if !options.skip_archive_count {
        probes.push(probe_archive_drift(
            &archive_drift,
            options.stale_threshold_pct,
            options.stale_threshold_abs,
        ));
    }

    MailboxHealthVerdict::from_components(
        sqlite_path,
        integrity,
        wal,
        recovery_lock,
        ownership,
        archive_drift,
        probes,
    )
}

// ============================================================================
// Individual probes
// ============================================================================

/// Probe: DB file exists and is non-zero size.
fn probe_db_file_exists(db_path: &Path) -> ProbeResult {
    if db_path.as_os_str() == ":memory:" {
        return ProbeResult::ok("db_exists", "In-memory database");
    }

    match std::fs::metadata(db_path) {
        Ok(meta) => {
            if meta.len() == 0 {
                ProbeResult::error("db_exists", "Database file is zero bytes")
            } else {
                ProbeResult::ok(
                    "db_exists",
                    format!("Database file exists ({} bytes)", meta.len()),
                )
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ProbeResult::error("db_exists", "Database file not found")
        }
        Err(e) => ProbeResult::error("db_exists", format!("Cannot stat database file: {e}")),
    }
}

/// Probe: DB file passes quick_check.
fn probe_db_file_sanity(db_path: &Path) -> ProbeResult {
    fn probe_duration_micros(start: std::time::Instant) -> u64 {
        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    if db_path.as_os_str() == ":memory:" {
        return ProbeResult::ok("db_sanity", "In-memory database (quick_check skipped)");
    }

    let start = std::time::Instant::now();
    match sqlite_primary_read_path_is_healthy(db_path) {
        Ok(true) => {}
        Ok(false) => {
            return ProbeResult::error("db_sanity", "Primary SQLite read-path health probe failed")
                .with_duration(probe_duration_micros(start));
        }
        Err(e) => {
            return ProbeResult::error(
                "db_sanity",
                format!("Cannot run primary SQLite read-path health probe: {e}"),
            )
            .with_duration(probe_duration_micros(start));
        }
    }

    match sqlite_compatibility_read_path_is_healthy(db_path) {
        Ok(true) => ProbeResult::ok(
            "db_sanity",
            "Primary SQLite read path and compatibility reopen probe passed",
        )
        .with_duration(probe_duration_micros(start)),
        Ok(false) => ProbeResult::error(
            "db_sanity",
            "Primary SQLite read path passed, but the compatibility reopen probe failed",
        )
        .with_duration(probe_duration_micros(start)),
        Err(e) => ProbeResult::error(
            "db_sanity",
            format!("Cannot run SQLite compatibility reopen probe: {e}"),
        )
        .with_duration(probe_duration_micros(start)),
    }
}

/// Probe: Archive directory is accessible (exists and readable).
fn probe_archive_accessible(archive_root: &Path) -> ProbeResult {
    match std::fs::read_dir(archive_root) {
        Ok(_) => ProbeResult::ok("archive_accessible", "Archive directory is readable"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ProbeResult::error("archive_accessible", "Archive directory not found")
        }
        Err(e) => ProbeResult::error("archive_accessible", format!("Cannot read archive: {e}")),
    }
}

/// Probe: Archive directory is writable.
fn probe_archive_writable(archive_root: &Path) -> ProbeResult {
    let test_file = archive_root.join(".write_probe");
    match std::fs::write(&test_file, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test_file);
            ProbeResult::ok("archive_writable", "Archive directory is writable")
        }
        Err(e) => ProbeResult::warn(
            "archive_writable",
            format!("Archive directory not writable: {e}"),
        ),
    }
}

/// Probe: Check WAL/SHM sidecar state.
fn probe_sidecar_state(sidecars: &MailboxSidecarState) -> ProbeResult {
    match (sidecars.wal_exists, sidecars.shm_exists) {
        (false, false) => ProbeResult::ok("sidecar_state", "No WAL/SHM sidecars present"),
        (true, true) => ProbeResult::ok("sidecar_state", "WAL and SHM sidecars present"),
        (true, false) => ProbeResult::warn_state(
            "sidecar_state",
            format!(
                "WAL exists without SHM (wal_bytes={})",
                sidecars.wal_bytes.unwrap_or(0)
            ),
            MailboxState::Suspect,
        ),
        (false, true) => ProbeResult::warn_state(
            "sidecar_state",
            format!(
                "SHM exists without WAL (shm_bytes={})",
                sidecars.shm_bytes.unwrap_or(0)
            ),
            MailboxState::Suspect,
        ),
    }
}

/// Probe: Check for recovery lock file.
fn probe_recovery_lock(lock_state: &MailboxRecoveryLockState) -> ProbeResult {
    if lock_state.active {
        ProbeResult::warn_state(
            "recovery_lock",
            lock_state.detail.clone(),
            MailboxState::Recovering,
        )
    } else if lock_state.exists {
        ProbeResult::warn("recovery_lock", lock_state.detail.clone())
    } else {
        ProbeResult::ok("recovery_lock", lock_state.detail.clone())
    }
}

fn probe_mailbox_ownership(ownership: &MailboxOwnershipState) -> ProbeResult {
    match ownership.disposition {
        MailboxOwnershipDisposition::Unowned => {
            ProbeResult::ok("mailbox_ownership", ownership.detail.clone())
        }
        MailboxOwnershipDisposition::ActiveOtherOwner => ProbeResult::warn_state(
            "mailbox_ownership",
            ownership.detail.clone(),
            MailboxState::Suspect,
        ),
        MailboxOwnershipDisposition::StaleLiveProcess
        | MailboxOwnershipDisposition::DeletedExecutable
        | MailboxOwnershipDisposition::SplitBrain => {
            ProbeResult::fatal("mailbox_ownership", ownership.detail.clone())
        }
    }
}

fn probe_integrity(integrity: &MailboxIntegrityVerdict) -> ProbeResult {
    match integrity.status {
        MailboxIntegrityStatus::Healthy => ProbeResult::ok("integrity", integrity.detail.clone()),
        MailboxIntegrityStatus::Suspect => {
            ProbeResult::warn_state("integrity", integrity.detail.clone(), MailboxState::Suspect)
        }
        MailboxIntegrityStatus::Broken => ProbeResult::error("integrity", integrity.detail.clone()),
        MailboxIntegrityStatus::Skipped => ProbeResult::ok("integrity", integrity.detail.clone()),
    }
}

fn inspect_archive_drift(db_path: &Path, archive_root: &Path) -> MailboxArchiveDriftVerdict {
    if db_path.as_os_str() == ":memory:" {
        return MailboxArchiveDriftVerdict::skipped(
            "Archive drift inspection skipped for in-memory database",
        );
    }

    let archive_inventory = scan_archive_message_inventory(archive_root);
    let db_inventory = match inspect_mailbox_db_inventory(db_path) {
        Ok(inventory) => inventory,
        Err(error) => {
            return MailboxArchiveDriftVerdict {
                state: MailboxArchiveDriftState::Unknown,
                archive_projects: archive_inventory.projects,
                archive_agents: archive_inventory.agents,
                archive_messages: archive_inventory.unique_message_ids,
                db_projects: 0,
                db_agents: 0,
                db_messages: 0,
                archive_latest_message_id: archive_inventory.latest_message_id,
                db_max_message_id: 0,
                missing_projects: Vec::new(),
                detail: format!("Cannot inspect DB inventory for archive drift: {error}"),
            };
        }
    };
    let missing_projects =
        archive_missing_project_identities(&archive_inventory, &db_inventory.project_identities);
    let archive_message_count = archive_inventory.unique_message_ids;
    let db_message_count = db_inventory.messages;
    let archive_messages_ahead = archive_message_count > db_message_count;
    let archive_latest_id_ahead =
        archive_inventory.latest_message_id.unwrap_or(0) > db_inventory.max_message_id;
    let archive_metadata_ahead = crate::pool::archive_metadata_advantage_is_decisive(
        archive_inventory.projects,
        archive_inventory.agents,
        archive_message_count,
        archive_inventory.latest_message_id,
        db_inventory.projects,
        db_inventory.agents,
        db_message_count,
        db_inventory.max_message_id,
        &missing_projects,
    );
    let archive_ahead = archive_messages_ahead || archive_latest_id_ahead || archive_metadata_ahead;
    let db_projects_ahead = db_inventory.projects > archive_inventory.projects;
    let db_agents_ahead = db_inventory.agents > archive_inventory.agents;
    let db_messages_ahead = db_message_count > archive_message_count;
    let db_latest_id_ahead =
        db_inventory.max_message_id > archive_inventory.latest_message_id.unwrap_or(0);
    let db_ahead = db_messages_ahead
        || db_latest_id_ahead
        || (!(archive_messages_ahead || archive_latest_id_ahead)
            && (db_projects_ahead || db_agents_ahead));
    let state = if archive_inventory.projects == 0
        && archive_inventory.agents == 0
        && archive_inventory.unique_message_ids == 0
        && db_inventory.projects == 0
        && db_inventory.agents == 0
        && db_inventory.messages == 0
    {
        MailboxArchiveDriftState::Aligned
    } else if archive_ahead {
        MailboxArchiveDriftState::ArchiveAhead
    } else if db_ahead {
        MailboxArchiveDriftState::DbAhead
    } else {
        MailboxArchiveDriftState::Aligned
    };
    MailboxArchiveDriftVerdict {
        state,
        archive_projects: archive_inventory.projects,
        archive_agents: archive_inventory.agents,
        archive_messages: archive_inventory.unique_message_ids,
        db_projects: db_inventory.projects,
        db_agents: db_inventory.agents,
        db_messages: db_inventory.messages,
        archive_latest_message_id: archive_inventory.latest_message_id,
        db_max_message_id: db_inventory.max_message_id,
        missing_projects: missing_projects.clone(),
        detail: match state {
            MailboxArchiveDriftState::Aligned => format!(
                "Archive and DB inventories align (archive projects={}, agents={}, messages={}; db projects={}, agents={}, messages={})",
                archive_inventory.projects,
                archive_inventory.agents,
                archive_inventory.unique_message_ids,
                db_inventory.projects,
                db_inventory.agents,
                db_inventory.messages
            ),
            MailboxArchiveDriftState::ArchiveAhead => format!(
                "Archive is ahead of DB (archive projects={}, agents={}, messages={}, latest_id={:?}; db projects={}, agents={}, messages={}, max_id={}; missing_projects={})",
                archive_inventory.projects,
                archive_inventory.agents,
                archive_inventory.unique_message_ids,
                archive_inventory.latest_message_id,
                db_inventory.projects,
                db_inventory.agents,
                db_inventory.messages,
                db_inventory.max_message_id,
                missing_projects.join(", ")
            ),
            MailboxArchiveDriftState::DbAhead => format!(
                "DB is ahead of archive (archive projects={}, agents={}, messages={}; db projects={}, agents={}, messages={})",
                archive_inventory.projects,
                archive_inventory.agents,
                archive_inventory.unique_message_ids,
                db_inventory.projects,
                db_inventory.agents,
                db_inventory.messages
            ),
            MailboxArchiveDriftState::Skipped | MailboxArchiveDriftState::Unknown => {
                "Archive drift state unavailable".to_string()
            }
        },
    }
}

fn probe_archive_drift(
    drift: &MailboxArchiveDriftVerdict,
    threshold_pct: f64,
    threshold_abs: usize,
) -> ProbeResult {
    match drift.state {
        MailboxArchiveDriftState::Skipped => {
            ProbeResult::ok("archive_db_parity", drift.detail.clone())
        }
        MailboxArchiveDriftState::Unknown => {
            ProbeResult::warn("archive_db_parity", drift.detail.clone())
        }
        MailboxArchiveDriftState::Aligned => {
            ProbeResult::ok("archive_db_parity", drift.detail.clone())
        }
        MailboxArchiveDriftState::ArchiveAhead => {
            let decisive_archive_ahead = drift.archive_projects > drift.db_projects
                || drift.archive_agents > drift.db_agents
                || drift.archive_latest_message_id.unwrap_or(0) > drift.db_max_message_id
                || !drift.missing_projects.is_empty();
            let diff = drift.archive_messages.saturating_sub(drift.db_messages);
            let max_count = drift.archive_messages.max(drift.db_messages);
            let pct = if max_count > 0 {
                diff as f64 / max_count as f64
            } else {
                0.0
            };
            if !decisive_archive_ahead && diff <= threshold_abs && pct <= threshold_pct {
                ProbeResult::ok("archive_db_parity", drift.detail.clone())
            } else {
                ProbeResult::warn_state(
                    "archive_db_parity",
                    drift.detail.clone(),
                    MailboxState::Stale,
                )
            }
        }
        MailboxArchiveDriftState::DbAhead => ProbeResult::warn_state(
            "archive_db_parity",
            drift.detail.clone(),
            MailboxState::Suspect,
        ),
    }
}

/// Probe: Schema populated check.
fn probe_schema_populated(db_path: &Path, archive_count: usize) -> ProbeResult {
    if db_path.as_os_str() == ":memory:" {
        return ProbeResult::ok(
            "schema_populated",
            "In-memory database (schema check skipped)",
        );
    }

    let path_str = db_path.display().to_string();
    let conn = match crate::DbConn::open_file(&path_str) {
        Ok(conn) => conn,
        Err(error) => {
            return ProbeResult::error(
                "schema_populated",
                format!("Cannot open database for schema check: {error}"),
            );
        }
    };

    let table_count = match conn.query_sync(
        "SELECT COUNT(*) AS table_count FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        &[],
    ) {
        Ok(rows) => rows
            .first()
            .and_then(|row| row.get_named::<i64>("table_count").ok())
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0),
        Err(error) => {
            return ProbeResult::error(
                "schema_populated",
                format!("Cannot query sqlite_master: {error}"),
            );
        }
    };

    let has_messages_table = conn
        .query_sync(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages'",
            &[],
        )
        .is_ok_and(|rows| !rows.is_empty());

    match (table_count, archive_count, has_messages_table) {
        (0, 0, _) => ProbeResult::ok(
            "schema_populated",
            "Database schema is empty and the archive is also empty",
        ),
        (0, archive, _) if archive > 0 => ProbeResult::error(
            "schema_populated",
            format!(
                "Database schema is empty (sqlite_master == 0) while archive has {archive} messages"
            ),
        ),
        (tables, _, false) if tables > 0 => ProbeResult::warn_state(
            "schema_populated",
            format!("Database has {tables} tables but no 'messages' table"),
            MailboxState::Suspect,
        ),
        (tables, _, true) => ProbeResult::ok(
            "schema_populated",
            format!("Database schema populated with {tables} tables including 'messages'"),
        ),
        _ => ProbeResult::ok("schema_populated", "Schema state accepted"),
    }
}

// ============================================================================
// Durability verification artifact (br-97gc6.5.2.6.6)
// ============================================================================

/// Fault class that a durability test scenario exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityFaultClass {
    /// DB file is missing or zero-length while archive is non-empty.
    EmptyDb,
    /// DB file fails `PRAGMA integrity_check` or has wrong schema.
    MalformedDb,
    /// DB file is locked by another process / busy timeout.
    BusyLock,
    /// The server binary or recovery binary is missing from PATH.
    DeletedBinary,
    /// Archive message count diverges from DB message count.
    ArchiveDrift,
    /// Recovery process was killed mid-operation (crash injection).
    AbruptTermination,
    /// Concurrent readers during background rebuild.
    DegradedConcurrentRead,
    /// WAL/SHM sidecars are inconsistent or orphaned.
    StaleSidecars,
    /// No fault injected (baseline/happy-path control).
    None,
}

impl DurabilityFaultClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyDb => "empty_db",
            Self::MalformedDb => "malformed_db",
            Self::BusyLock => "busy_lock",
            Self::DeletedBinary => "deleted_binary",
            Self::ArchiveDrift => "archive_drift",
            Self::AbruptTermination => "abrupt_termination",
            Self::DegradedConcurrentRead => "degraded_concurrent_read",
            Self::StaleSidecars => "stale_sidecars",
            Self::None => "none",
        }
    }
}

impl std::fmt::Display for DurabilityFaultClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a durability test scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityTestOutcome {
    /// Test passed: the system detected and handled the fault correctly.
    Pass,
    /// Test failed: the system did not handle the fault as expected.
    Fail,
    /// Test was skipped (prerequisites not met, e.g. missing binary).
    Skip,
    /// Test encountered an unexpected error during setup or teardown.
    Error,
}

impl DurabilityTestOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for DurabilityTestOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured artifact emitted by every durability test scenario.
///
/// Consumed by CI gates, the verification matrix, and the residual-risk
/// register. Every durability test — unit, E2E, or chaos — must emit
/// exactly one of these per scenario run so downstream tooling can
/// aggregate pass/fail evidence without parsing log output.
///
/// # Fields by audience
///
/// **Machine-facing** (for CI gates and aggregation):
/// `scenario_id`, `fault_class`, `outcome`, `started_at_micros`,
/// `finished_at_micros`, `durability_state_before`, `durability_state_after`,
/// `messages_before`, `messages_after`, `data_loss_detected`.
///
/// **Operator-facing** (for triage and investigation):
/// `description`, `failure_reason`, `forensic_bundle_path`, `rerun_hint`,
/// `probes_before`, `probes_after`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurabilityTestArtifact {
    // ── scenario identity ────────────────────────────────────────────
    /// Stable identifier for the test scenario (e.g., `"empty_db_with_archive"`).
    pub scenario_id: String,
    /// Human-readable description of what this scenario tests.
    pub description: String,
    /// The fault class exercised by this scenario.
    pub fault_class: DurabilityFaultClass,

    // ── timing ───────────────────────────────────────────────────────
    /// When the scenario started (microseconds since epoch).
    pub started_at_micros: i64,
    /// When the scenario finished (microseconds since epoch).
    pub finished_at_micros: i64,

    // ── mailbox state transitions ────────────────────────────────────
    /// Mailbox durability state observed before fault injection.
    pub durability_state_before: DurabilityState,
    /// Mailbox durability state observed after fault handling / recovery.
    pub durability_state_after: DurabilityState,
    /// Mailbox state (fine-grained) before fault injection.
    pub mailbox_state_before: MailboxState,
    /// Mailbox state (fine-grained) after fault handling / recovery.
    pub mailbox_state_after: MailboxState,

    // ── ownership evidence ───────────────────────────────────────────
    /// Ownership disposition before fault injection.
    pub owner_disposition_before: Option<MailboxOwnershipDisposition>,
    /// Ownership disposition after fault handling.
    pub owner_disposition_after: Option<MailboxOwnershipDisposition>,

    // ── archive / DB facts ───────────────────────────────────────────
    /// Archive message count before fault injection.
    pub archive_messages_before: Option<usize>,
    /// Archive message count after fault handling.
    pub archive_messages_after: Option<usize>,
    /// DB message count before fault injection.
    pub db_messages_before: Option<usize>,
    /// DB message count after fault handling.
    pub db_messages_after: Option<usize>,
    /// Archive drift state after recovery.
    pub archive_drift_after: Option<MailboxArchiveDriftState>,

    // ── data integrity assertions ────────────────────────────────────
    /// Whether any data loss was detected (messages lost, not recoverable).
    pub data_loss_detected: bool,
    /// Number of messages that were lost (0 if none).
    pub messages_lost: usize,

    // ── outcome ──────────────────────────────────────────────────────
    /// Test outcome.
    pub outcome: DurabilityTestOutcome,
    /// If outcome is `Fail` or `Error`, the reason.
    pub failure_reason: Option<String>,

    // ── forensic / recovery artifacts ────────────────────────────────
    /// Path to the forensic bundle captured during this scenario, if any.
    pub forensic_bundle_path: Option<PathBuf>,
    /// Probe results before fault injection (for detailed triage).
    pub probes_before: Vec<ProbeResult>,
    /// Probe results after fault handling (for detailed triage).
    pub probes_after: Vec<ProbeResult>,

    // ── rerun hints ──────────────────────────────────────────────────
    /// Shell command to reproduce this scenario in isolation.
    pub rerun_hint: Option<String>,
}

impl DurabilityTestArtifact {
    /// Create a new artifact with the scenario identity and fault class.
    /// Timing fields are initialized to now; call [`Self::finish`] when done.
    #[must_use]
    pub fn begin(
        scenario_id: impl Into<String>,
        description: impl Into<String>,
        fault_class: DurabilityFaultClass,
    ) -> Self {
        let now = mcp_agent_mail_core::timestamps::now_micros();
        Self {
            scenario_id: scenario_id.into(),
            description: description.into(),
            fault_class,
            started_at_micros: now,
            finished_at_micros: 0,
            durability_state_before: DurabilityState::Healthy,
            durability_state_after: DurabilityState::Healthy,
            mailbox_state_before: MailboxState::Healthy,
            mailbox_state_after: MailboxState::Healthy,
            owner_disposition_before: None,
            owner_disposition_after: None,
            archive_messages_before: None,
            archive_messages_after: None,
            db_messages_before: None,
            db_messages_after: None,
            archive_drift_after: None,
            data_loss_detected: false,
            messages_lost: 0,
            outcome: DurabilityTestOutcome::Error, // default until finish()
            failure_reason: None,
            forensic_bundle_path: None,
            probes_before: Vec::new(),
            probes_after: Vec::new(),
            rerun_hint: None,
        }
    }

    /// Record the "before" snapshot from a health verdict.
    pub fn record_before(&mut self, verdict: &MailboxHealthVerdict) {
        self.mailbox_state_before = verdict.state;
        self.durability_state_before = DurabilityState::from_mailbox_state(verdict.state);
        self.owner_disposition_before = Some(verdict.ownership.disposition);
        self.probes_before.clone_from(&verdict.probes);
    }

    /// Record the "after" snapshot from a health verdict.
    pub fn record_after(&mut self, verdict: &MailboxHealthVerdict) {
        self.mailbox_state_after = verdict.state;
        self.durability_state_after = DurabilityState::from_mailbox_state(verdict.state);
        self.owner_disposition_after = Some(verdict.ownership.disposition);
        self.archive_drift_after = Some(verdict.archive_drift.state);
        self.probes_after.clone_from(&verdict.probes);
    }

    /// Mark the artifact as finished with a pass/fail outcome.
    pub fn finish(&mut self, outcome: DurabilityTestOutcome) {
        self.finished_at_micros = mcp_agent_mail_core::timestamps::now_micros();
        self.outcome = outcome;
    }

    /// Mark as failed with a reason.
    pub fn fail(&mut self, reason: impl Into<String>) {
        self.finish(DurabilityTestOutcome::Fail);
        self.failure_reason = Some(reason.into());
    }

    /// Duration of the scenario in microseconds.
    #[must_use]
    pub fn duration_micros(&self) -> i64 {
        self.finished_at_micros
            .saturating_sub(self.started_at_micros)
    }

    /// Whether the after-state is at least as good as the before-state
    /// (no regression).
    #[must_use]
    pub fn no_state_regression(&self) -> bool {
        self.durability_state_after.severity() <= self.durability_state_before.severity()
    }

    /// Serialize to JSON for artifact logging.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| format!("{self:?}"))
    }

    /// Serialize to a single compact JSON line for JSONL artifact logs.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| format!("{self:?}"))
    }
}

impl std::fmt::Display for DurabilityTestArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} ({}) → {} in {}ms",
            self.outcome,
            self.scenario_id,
            self.fault_class,
            self.durability_state_after,
            self.duration_micros() / 1000,
        )
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict_from_probes(
        probes: Vec<ProbeResult>,
        recovery_lock_active: bool,
    ) -> MailboxHealthVerdict {
        MailboxHealthVerdict::from_components(
            MailboxSqlitePathVerdict {
                database_url: "sqlite:///./storage.sqlite3".to_string(),
                configured_path: Some("./storage.sqlite3".to_string()),
                canonical_path: Some("./storage.sqlite3".to_string()),
                used_absolute_fallback: false,
                detail: "test sqlite path".to_string(),
            },
            MailboxIntegrityVerdict {
                status: MailboxIntegrityStatus::Skipped,
                metrics: crate::integrity::integrity_metrics(),
                check: None,
                detail: "test integrity".to_string(),
            },
            MailboxSidecarState::default(),
            MailboxRecoveryLockState {
                lock_path: "storage.sqlite3.recovery.lock".to_string(),
                exists: recovery_lock_active,
                active: recovery_lock_active,
                pid: recovery_lock_active.then_some(1234),
                detail: if recovery_lock_active {
                    "Recovery lock held by PID 1234".to_string()
                } else {
                    "No recovery lock present".to_string()
                },
            },
            MailboxOwnershipState {
                disposition: MailboxOwnershipDisposition::Unowned,
                storage_lock_path: "storage/.mailbox.activity.lock".to_string(),
                sqlite_lock_path: "storage.sqlite3.activity.lock".to_string(),
                processes: Vec::new(),
                competing_pids: Vec::new(),
                supervised_restart_required: false,
                detail: "test mailbox ownership".to_string(),
            },
            MailboxArchiveDriftVerdict::skipped("test archive drift"),
            probes,
        )
    }

    #[test]
    fn state_severity_ordering() {
        assert!(MailboxState::Escalate.severity() > MailboxState::Broken.severity());
        assert!(MailboxState::Broken.severity() > MailboxState::Recovering.severity());
        assert!(MailboxState::Recovering.severity() > MailboxState::Suspect.severity());
        assert!(MailboxState::Suspect.severity() > MailboxState::Stale.severity());
        assert!(MailboxState::Stale.severity() > MailboxState::DegradedReadOnly.severity());
        assert!(MailboxState::DegradedReadOnly.severity() > MailboxState::Healthy.severity());
    }

    #[test]
    fn state_max_severity() {
        assert_eq!(
            MailboxState::Healthy.max_severity(MailboxState::Stale),
            MailboxState::Stale
        );
        assert_eq!(
            MailboxState::Broken.max_severity(MailboxState::Healthy),
            MailboxState::Broken
        );
        assert_eq!(
            MailboxState::Suspect.max_severity(MailboxState::Stale),
            MailboxState::Suspect
        );
    }

    #[test]
    fn state_allows_reads_writes() {
        assert!(MailboxState::Healthy.allows_reads());
        assert!(MailboxState::Healthy.allows_writes());

        assert!(MailboxState::Stale.allows_reads());
        assert!(!MailboxState::Stale.allows_writes());

        assert!(MailboxState::Suspect.allows_reads());
        assert!(!MailboxState::Suspect.allows_writes());

        assert!(!MailboxState::Broken.allows_reads());
        assert!(!MailboxState::Broken.allows_writes());

        assert!(!MailboxState::Recovering.allows_reads());
        assert!(!MailboxState::Recovering.allows_writes());

        assert!(MailboxState::DegradedReadOnly.allows_reads());
        assert!(!MailboxState::DegradedReadOnly.allows_writes());

        assert!(!MailboxState::Escalate.allows_reads());
        assert!(!MailboxState::Escalate.allows_writes());
    }

    #[test]
    fn probe_result_constructors() {
        let ok = ProbeResult::ok("test", "detail");
        assert!(ok.passed);
        assert_eq!(ok.severity, ProbeSeverity::Info);
        assert_eq!(ok.impact_state, MailboxState::Healthy);

        let warn = ProbeResult::warn("test", "detail");
        assert!(!warn.passed);
        assert_eq!(warn.severity, ProbeSeverity::Warning);
        assert_eq!(warn.impact_state, MailboxState::Suspect);

        let err = ProbeResult::error("test", "detail");
        assert!(!err.passed);
        assert_eq!(err.severity, ProbeSeverity::Error);
        assert_eq!(err.impact_state, MailboxState::Broken);

        let fatal = ProbeResult::fatal("test", "detail");
        assert!(!fatal.passed);
        assert_eq!(fatal.severity, ProbeSeverity::Fatal);
        assert_eq!(fatal.impact_state, MailboxState::Escalate);
    }

    #[test]
    fn verdict_from_all_passing_probes() {
        let probes = vec![
            ProbeResult::ok("db_exists", "ok"),
            ProbeResult::ok("db_sanity", "ok"),
            ProbeResult::ok("archive_accessible", "ok"),
        ];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Healthy);
        assert_eq!(verdict.failure_count(), 0);
    }

    #[test]
    fn verdict_from_warning_probes_gives_suspect() {
        let probes = vec![
            ProbeResult::ok("db_exists", "ok"),
            ProbeResult::warn("sidecar_state", "WAL without SHM"),
        ];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Suspect);
        assert_eq!(verdict.warning_count(), 1);
    }

    #[test]
    fn verdict_from_stale_warning_gives_stale() {
        let probes = vec![
            ProbeResult::ok("db_exists", "ok"),
            ProbeResult::warn_state(
                "archive_db_parity",
                "Archive ahead of DB",
                MailboxState::Stale,
            ),
        ];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Stale);
    }

    #[test]
    fn verdict_db_ahead_drift_does_not_prefer_archive_snapshots_when_it_is_only_failure() {
        let probes = vec![
            ProbeResult::ok("db_exists", "ok"),
            ProbeResult::warn_state(
                "archive_db_parity",
                "DB is ahead of archive",
                MailboxState::Suspect,
            ),
        ];
        let mut verdict = verdict_from_probes(probes, false);
        verdict.archive_drift = MailboxArchiveDriftVerdict {
            state: MailboxArchiveDriftState::DbAhead,
            archive_projects: 0,
            archive_agents: 0,
            archive_messages: 1,
            db_projects: 1,
            db_agents: 2,
            db_messages: 2,
            archive_latest_message_id: Some(1),
            db_max_message_id: 2,
            missing_projects: Vec::new(),
            detail: "DB is ahead of archive by 1 message".to_string(),
        };

        assert!(!verdict_prefers_archive_snapshot_reads(&verdict));
    }

    #[test]
    fn verdict_db_ahead_drift_still_prefers_archive_snapshots_when_other_failures_exist() {
        let probes = vec![
            ProbeResult::warn_state(
                "archive_db_parity",
                "DB is ahead of archive",
                MailboxState::Suspect,
            ),
            ProbeResult::warn("sidecar_state", "WAL without SHM"),
        ];
        let mut verdict = verdict_from_probes(probes, false);
        verdict.archive_drift = MailboxArchiveDriftVerdict {
            state: MailboxArchiveDriftState::DbAhead,
            archive_projects: 0,
            archive_agents: 0,
            archive_messages: 1,
            db_projects: 1,
            db_agents: 2,
            db_messages: 2,
            archive_latest_message_id: Some(1),
            db_max_message_id: 2,
            missing_projects: Vec::new(),
            detail: "DB is ahead of archive by 1 message".to_string(),
        };

        assert!(verdict_prefers_archive_snapshot_reads(&verdict));
    }

    #[test]
    fn verdict_db_sanity_only_failure_does_not_force_archive_when_primary_read_path_is_healthy() {
        let probes = vec![ProbeResult::error(
            "db_sanity",
            "compatibility reopen failed",
        )];
        let verdict = verdict_from_probes(probes, false);
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("primary-healthy.sqlite3");
        let conn = crate::DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        drop(conn);

        assert!(
            !verdict_prefers_archive_snapshot_reads_for_primary_read_surface(&verdict, &db_path)
        );
    }

    #[test]
    fn verdict_uses_explicit_probe_impact_state_not_probe_name() {
        let probes = vec![ProbeResult::warn_state(
            "totally_unrelated_name",
            "explicit stale signal",
            MailboxState::Stale,
        )];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Stale);
    }

    #[test]
    fn verdict_from_error_probes_gives_broken() {
        let probes = vec![ProbeResult::error("db_exists", "Zero byte file")];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Broken);
    }

    #[test]
    fn verdict_from_fatal_probes_gives_escalate() {
        let probes = vec![ProbeResult::fatal(
            "recovery",
            "Multiple recovery attempts failed",
        )];
        let verdict = verdict_from_probes(probes, false);
        assert_eq!(verdict.state, MailboxState::Escalate);
    }

    #[test]
    fn recovery_lock_takes_precedence() {
        let probes = vec![
            ProbeResult::ok("db_exists", "ok"),
            ProbeResult::ok("db_sanity", "ok"),
        ];
        let verdict = verdict_from_probes(probes, true);
        assert_eq!(verdict.state, MailboxState::Recovering);
    }

    #[test]
    fn inspect_archive_drift_prefers_db_ahead_when_archive_only_has_metadata_advantage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&msg_dir).expect("create message dir");
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .expect("write agent metadata");
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .expect("write archive message");

        crate::reconstruct::reconstruct_from_archive(&db_path, &storage_root)
            .expect("seed live db from archive");

        let conn = crate::DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                crate::sqlmodel::Value::BigInt(3),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::Text("BlueLake".to_string()),
                crate::sqlmodel::Value::Text("coder".to_string()),
                crate::sqlmodel::Value::Text("test".to_string()),
                crate::sqlmodel::Value::Text(String::new()),
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::Text("auto".to_string()),
                crate::sqlmodel::Value::Text("auto".to_string()),
            ],
        )
        .expect("insert recipient");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::Text("t2".to_string()),
                crate::sqlmodel::Value::Text("Second".to_string()),
                crate::sqlmodel::Value::Text("second body".to_string()),
                crate::sqlmodel::Value::Text("normal".to_string()),
                crate::sqlmodel::Value::BigInt(0),
                crate::sqlmodel::Value::BigInt(2_000_000),
                crate::sqlmodel::Value::Text("[]".to_string()),
                crate::sqlmodel::Value::Text(r#"{"to":["BlueLake"],"cc":[],"bcc":[]}"#.to_string()),
            ],
        )
        .expect("insert newer live message");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, ack_ts, read_ts) VALUES (?, ?, ?, NULL, NULL)",
            &[
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(3),
                crate::sqlmodel::Value::Text("to".to_string()),
            ],
        )
        .expect("insert message recipient");
        drop(conn);

        let archive_only_project = storage_root.join("projects").join("archive-only-project");
        let archive_only_agent = archive_only_project.join("agents").join("ArchiveGhost");
        std::fs::create_dir_all(&archive_only_agent).expect("create archive-only project dir");
        std::fs::write(
            archive_only_project.join("project.json"),
            r#"{"slug":"archive-only-project","human_key":"/archive-only-project","created_at":0}"#,
        )
        .expect("write archive-only project metadata");
        std::fs::write(
            archive_only_agent.join("profile.json"),
            r#"{"agent_name":"ArchiveGhost","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write archive-only agent metadata");

        let drift = inspect_archive_drift(&db_path, &storage_root);
        assert_eq!(drift.state, MailboxArchiveDriftState::DbAhead);
        assert!(
            drift.detail.contains("DB is ahead of archive"),
            "expected db-ahead detail, got: {}",
            drift.detail
        );
    }

    #[test]
    fn fatal_probe_beats_recovery_lock_precedence() {
        let probes = vec![ProbeResult::fatal(
            "mailbox_ownership",
            "mailbox ownership is split-brain",
        )];
        let verdict = verdict_from_probes(probes, true);
        assert_eq!(verdict.state, MailboxState::Escalate);
    }

    #[test]
    fn mailbox_ownership_probe_maps_dispositions_to_expected_states() {
        let ok = probe_mailbox_ownership(&MailboxOwnershipState {
            disposition: MailboxOwnershipDisposition::Unowned,
            storage_lock_path: "storage/.mailbox.activity.lock".to_string(),
            sqlite_lock_path: "storage.sqlite3.activity.lock".to_string(),
            processes: Vec::new(),
            competing_pids: Vec::new(),
            supervised_restart_required: false,
            detail: "clean".to_string(),
        });
        assert!(ok.passed);

        let warn = probe_mailbox_ownership(&MailboxOwnershipState {
            disposition: MailboxOwnershipDisposition::ActiveOtherOwner,
            storage_lock_path: "storage/.mailbox.activity.lock".to_string(),
            sqlite_lock_path: "storage.sqlite3.activity.lock".to_string(),
            processes: Vec::new(),
            competing_pids: vec![1234],
            supervised_restart_required: false,
            detail: "other owner".to_string(),
        });
        assert_eq!(warn.severity, ProbeSeverity::Warning);
        assert_eq!(warn.impact_state, MailboxState::Suspect);

        let fatal = probe_mailbox_ownership(&MailboxOwnershipState {
            disposition: MailboxOwnershipDisposition::SplitBrain,
            storage_lock_path: "storage/.mailbox.activity.lock".to_string(),
            sqlite_lock_path: "storage.sqlite3.activity.lock".to_string(),
            processes: Vec::new(),
            competing_pids: vec![1234, 5678],
            supervised_restart_required: true,
            detail: "split-brain".to_string(),
        });
        assert_eq!(fatal.severity, ProbeSeverity::Fatal);
        assert_eq!(fatal.impact_state, MailboxState::Escalate);
    }

    #[test]
    fn compute_mailbox_verdict_marks_invalid_database_url_broken() {
        let archive_root = tempfile::tempdir().expect("tempdir");
        let verdict = compute_mailbox_verdict(
            "postgresql://not-a-sqlite-path",
            archive_root.path(),
            &VerdictOptions::default(),
        );
        assert_eq!(verdict.state, MailboxState::Broken);
        assert!(
            verdict
                .probes
                .iter()
                .any(|probe| probe.name == "db_path_resolution"),
            "invalid DATABASE_URL should surface a path-resolution probe"
        );
    }

    #[test]
    fn verdict_options_default() {
        let opts = VerdictOptions::default();
        assert!(!opts.skip_archive_count);
        assert!(!opts.skip_integrity_check);
        assert!(opts.check_recovery_lock);
        assert!((opts.stale_threshold_pct - 0.05).abs() < f64::EPSILON);
        assert_eq!(opts.stale_threshold_abs, 100);
    }

    #[test]
    fn verdict_options_fast() {
        let opts = VerdictOptions::fast();
        assert!(opts.skip_archive_count);
        assert!(opts.skip_integrity_check);
        assert!(opts.check_recovery_lock);
    }

    #[test]
    fn state_serialization() {
        let state = MailboxState::DegradedReadOnly;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"degraded_read_only\"");

        let parsed: MailboxState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn probe_db_file_exists_memory() {
        let probe = probe_db_file_exists(Path::new(":memory:"));
        assert!(probe.passed);
        assert!(probe.detail.contains("In-memory"));
    }

    #[test]
    fn probe_db_file_sanity_memory() {
        let probe = probe_db_file_sanity(Path::new(":memory:"));
        assert!(probe.passed);
        assert!(probe.detail.contains("In-memory"));
    }

    #[test]
    fn probe_db_file_exists_missing() {
        let probe = probe_db_file_exists(Path::new("/nonexistent/path/db.sqlite3"));
        assert!(!probe.passed);
        assert_eq!(probe.severity, ProbeSeverity::Error);
    }

    #[test]
    fn probe_schema_populated_memory() {
        let probe = probe_schema_populated(Path::new(":memory:"), 0);
        assert!(probe.passed);
        assert!(probe.detail.contains("In-memory"));
    }

    #[test]
    fn compute_mailbox_verdict_skips_file_and_archive_drift_probes_for_in_memory_database() {
        let archive_root = tempfile::tempdir().expect("tempdir");
        let project_dir = archive_root.path().join("projects").join("demo-project");
        let message_dir = project_dir.join("messages").join("2026").join("04");

        std::fs::create_dir_all(&message_dir).expect("create message dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"demo-project","human_key":"/demo/project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            message_dir.join("2026-04-10T12-00-00Z__hello__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"hello\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-04-10T12:00:00Z\",\"attachments\":[]}\n---\n\nbody\n",
        )
        .expect("write archive message");

        let verdict = compute_mailbox_verdict(
            "sqlite:///:memory:",
            archive_root.path(),
            &VerdictOptions::default(),
        );

        assert_eq!(verdict.state, MailboxState::Healthy);
        assert_eq!(
            verdict.archive_drift.state,
            MailboxArchiveDriftState::Skipped
        );
        assert!(
            verdict.archive_drift.detail.contains("in-memory")
                || verdict.archive_drift.detail.contains("In-memory")
        );

        let db_sanity = verdict
            .probes
            .iter()
            .find(|probe| probe.name == "db_sanity")
            .expect("db_sanity probe present");
        assert!(db_sanity.passed);
        assert!(db_sanity.detail.contains("In-memory"));

        let archive_parity = verdict
            .probes
            .iter()
            .find(|probe| probe.name == "archive_db_parity")
            .expect("archive parity probe present");
        assert!(archive_parity.passed);
        assert!(archive_parity.detail.contains("skipped"));
    }

    #[test]
    fn probe_schema_populated_empty_db_empty_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("empty.sqlite3");
        // Create an empty but valid SQLite file
        let conn = crate::DbConn::open_file(db_path.to_str().unwrap()).expect("open db");
        drop(conn);

        let probe = probe_schema_populated(&db_path, 0);
        assert!(probe.passed);
        assert!(probe.detail.contains("empty") || probe.detail.contains("schema"));
    }

    #[test]
    fn probe_schema_populated_empty_db_with_archive_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("empty_with_archive.sqlite3");
        // Create an empty but valid SQLite file
        let conn = crate::DbConn::open_file(db_path.to_str().unwrap()).expect("open db");
        drop(conn);

        // Simulate archive having 100 messages
        let probe = probe_schema_populated(&db_path, 100);
        assert!(!probe.passed);
        assert_eq!(probe.severity, ProbeSeverity::Error);
        assert!(
            probe.detail.contains("sqlite_master == 0")
                || probe.detail.contains("requires reconstruct"),
            "unexpected detail: {}",
            probe.detail,
        );
    }

    // ── DurabilityState tests ────────────────────────────────────────

    #[test]
    fn durability_severity_ordering() {
        assert!(DurabilityState::Corrupt.severity() > DurabilityState::Recovering.severity());
        assert!(
            DurabilityState::Recovering.severity() > DurabilityState::DegradedReadOnly.severity()
        );
        assert!(DurabilityState::DegradedReadOnly.severity() > DurabilityState::Healthy.severity());
    }

    #[test]
    fn durability_allows_reads() {
        assert!(DurabilityState::Healthy.allows_reads());
        assert!(DurabilityState::DegradedReadOnly.allows_reads());
        assert!(!DurabilityState::Recovering.allows_reads());
        assert!(!DurabilityState::Corrupt.allows_reads());
    }

    #[test]
    fn durability_allows_writes() {
        assert!(DurabilityState::Healthy.allows_writes());
        assert!(!DurabilityState::DegradedReadOnly.allows_writes());
        assert!(!DurabilityState::Recovering.allows_writes());
        assert!(!DurabilityState::Corrupt.allows_writes());
    }

    #[test]
    fn durability_allows_recovery_start() {
        assert!(!DurabilityState::Healthy.allows_recovery_start());
        assert!(DurabilityState::DegradedReadOnly.allows_recovery_start());
        assert!(!DurabilityState::Recovering.allows_recovery_start());
        assert!(DurabilityState::Corrupt.allows_recovery_start());
    }

    #[test]
    fn durability_is_degraded() {
        assert!(!DurabilityState::Healthy.is_degraded());
        assert!(DurabilityState::DegradedReadOnly.is_degraded());
        assert!(DurabilityState::Recovering.is_degraded());
        assert!(DurabilityState::Corrupt.is_degraded());
    }

    #[test]
    fn durability_display() {
        assert_eq!(DurabilityState::Healthy.to_string(), "healthy");
        assert_eq!(
            DurabilityState::DegradedReadOnly.to_string(),
            "degraded_read_only"
        );
        assert_eq!(DurabilityState::Recovering.to_string(), "recovering");
        assert_eq!(DurabilityState::Corrupt.to_string(), "corrupt");
    }

    #[test]
    fn durability_serde_roundtrip() {
        for &state in &[
            DurabilityState::Healthy,
            DurabilityState::DegradedReadOnly,
            DurabilityState::Recovering,
            DurabilityState::Corrupt,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: DurabilityState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn durability_from_mailbox_state_mapping() {
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Healthy),
            DurabilityState::Healthy
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Stale),
            DurabilityState::DegradedReadOnly
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Suspect),
            DurabilityState::DegradedReadOnly
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::DegradedReadOnly),
            DurabilityState::DegradedReadOnly
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Recovering),
            DurabilityState::Recovering
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Broken),
            DurabilityState::Corrupt
        );
        assert_eq!(
            DurabilityState::from_mailbox_state(MailboxState::Escalate),
            DurabilityState::Corrupt
        );
    }

    #[test]
    fn durability_floor_from_health_level() {
        use mcp_agent_mail_core::HealthLevel;
        assert_eq!(
            DurabilityState::floor_from_health_level(HealthLevel::Green),
            DurabilityState::Healthy
        );
        assert_eq!(
            DurabilityState::floor_from_health_level(HealthLevel::Yellow),
            DurabilityState::Healthy
        );
        assert_eq!(
            DurabilityState::floor_from_health_level(HealthLevel::Red),
            DurabilityState::DegradedReadOnly
        );
    }

    #[test]
    fn durability_max_severity() {
        assert_eq!(
            DurabilityState::Healthy.max_severity(DurabilityState::Corrupt),
            DurabilityState::Corrupt
        );
        assert_eq!(
            DurabilityState::Corrupt.max_severity(DurabilityState::Healthy),
            DurabilityState::Corrupt
        );
        assert_eq!(
            DurabilityState::DegradedReadOnly.max_severity(DurabilityState::Recovering),
            DurabilityState::Recovering
        );
    }

    #[test]
    fn durability_self_transitions_are_idempotent() {
        for &state in &[
            DurabilityState::Healthy,
            DurabilityState::DegradedReadOnly,
            DurabilityState::Recovering,
            DurabilityState::Corrupt,
        ] {
            let trigger = validate_durability_transition(state, state)
                .expect("self-transition should succeed");
            assert_eq!(trigger, "idempotent");
        }
    }

    #[test]
    fn durability_valid_transitions() {
        for &(from, to) in &[
            (DurabilityState::Healthy, DurabilityState::DegradedReadOnly),
            (DurabilityState::Healthy, DurabilityState::Corrupt),
            (DurabilityState::DegradedReadOnly, DurabilityState::Healthy),
            (
                DurabilityState::DegradedReadOnly,
                DurabilityState::Recovering,
            ),
            (DurabilityState::DegradedReadOnly, DurabilityState::Corrupt),
            (DurabilityState::Recovering, DurabilityState::Healthy),
            (
                DurabilityState::Recovering,
                DurabilityState::DegradedReadOnly,
            ),
            (DurabilityState::Recovering, DurabilityState::Corrupt),
            (DurabilityState::Corrupt, DurabilityState::Recovering),
        ] {
            validate_durability_transition(from, to)
                .unwrap_or_else(|err| panic!("expected {from} -> {to} to be valid: {err}"));
        }
    }

    #[test]
    fn durability_invalid_transitions() {
        for &(from, to) in &[
            (DurabilityState::Healthy, DurabilityState::Recovering),
            (DurabilityState::Corrupt, DurabilityState::Healthy),
            (DurabilityState::Corrupt, DurabilityState::DegradedReadOnly),
            (DurabilityState::Recovering, DurabilityState::Recovering),
        ] {
            // Skip the self-transition case (Recovering->Recovering is idempotent)
            if from == to {
                continue;
            }
            validate_durability_transition(from, to)
                .expect_err(&format!("expected {from} -> {to} to be invalid"));
        }
    }

    #[test]
    fn durability_corrupt_only_exits_to_recovering() {
        let corrupt_exits: Vec<_> = DURABILITY_TRANSITIONS
            .iter()
            .filter(|t| t.from == DurabilityState::Corrupt)
            .collect();
        assert_eq!(corrupt_exits.len(), 1);
        assert_eq!(corrupt_exits[0].to, DurabilityState::Recovering);
    }

    #[test]
    fn durability_healthy_cannot_directly_recover() {
        let err =
            validate_durability_transition(DurabilityState::Healthy, DurabilityState::Recovering)
                .expect_err("Healthy -> Recovering must be invalid");
        assert_eq!(err, "invalid durability state transition");
    }

    #[test]
    fn durability_transition_count() {
        assert_eq!(
            DURABILITY_TRANSITIONS.len(),
            9,
            "exactly 9 valid transitions"
        );
    }

    #[test]
    fn durability_every_state_has_at_least_one_exit() {
        for &state in &[
            DurabilityState::Healthy,
            DurabilityState::DegradedReadOnly,
            DurabilityState::Recovering,
            DurabilityState::Corrupt,
        ] {
            let exits = DURABILITY_TRANSITIONS
                .iter()
                .filter(|t| t.from == state)
                .count();
            assert!(exits >= 1, "{state} must have at least one exit transition");
        }
    }

    // ── Exhaustive DurabilityState transition matrix tests ──────────

    const ALL_DURABILITY_STATES: [DurabilityState; 4] = [
        DurabilityState::Healthy,
        DurabilityState::DegradedReadOnly,
        DurabilityState::Recovering,
        DurabilityState::Corrupt,
    ];

    #[test]
    fn durability_exhaustive_4x4_transition_matrix() {
        // Classify every (from, to) pair in the 4×4 matrix.
        // Valid non-self transitions that MUST succeed:
        let valid_pairs: &[(DurabilityState, DurabilityState)] = &[
            (DurabilityState::Healthy, DurabilityState::DegradedReadOnly),
            (DurabilityState::Healthy, DurabilityState::Corrupt),
            (DurabilityState::DegradedReadOnly, DurabilityState::Healthy),
            (
                DurabilityState::DegradedReadOnly,
                DurabilityState::Recovering,
            ),
            (DurabilityState::DegradedReadOnly, DurabilityState::Corrupt),
            (DurabilityState::Recovering, DurabilityState::Healthy),
            (
                DurabilityState::Recovering,
                DurabilityState::DegradedReadOnly,
            ),
            (DurabilityState::Recovering, DurabilityState::Corrupt),
            (DurabilityState::Corrupt, DurabilityState::Recovering),
        ];

        // Invalid non-self transitions that MUST fail:
        let invalid_pairs: &[(DurabilityState, DurabilityState)] = &[
            (DurabilityState::Healthy, DurabilityState::Recovering),
            (DurabilityState::Corrupt, DurabilityState::Healthy),
            (DurabilityState::Corrupt, DurabilityState::DegradedReadOnly),
        ];

        // Self-transitions (idempotent):
        for &state in &ALL_DURABILITY_STATES {
            let trigger = validate_durability_transition(state, state)
                .unwrap_or_else(|e| panic!("{state}->{state} should be idempotent: {e}"));
            assert_eq!(trigger, "idempotent", "{state}->{state}");
        }

        for &(from, to) in valid_pairs {
            let trigger = validate_durability_transition(from, to)
                .unwrap_or_else(|e| panic!("{from}->{to} should be valid: {e}"));
            assert_ne!(
                trigger, "idempotent",
                "{from}->{to} is not a self-transition"
            );
        }

        for &(from, to) in invalid_pairs {
            validate_durability_transition(from, to)
                .expect_err(&format!("{from}->{to} should be invalid"));
        }

        // Verify we covered all 16 cells: 4 self + 9 valid + 3 invalid = 16
        assert_eq!(
            4 + valid_pairs.len() + invalid_pairs.len(),
            16,
            "must cover all 4×4 = 16 cells"
        );
    }

    #[test]
    fn durability_no_duplicate_transitions() {
        for (i, a) in DURABILITY_TRANSITIONS.iter().enumerate() {
            for (j, b) in DURABILITY_TRANSITIONS.iter().enumerate() {
                if i != j {
                    assert!(
                        !(a.from == b.from && a.to == b.to),
                        "duplicate transition: {}->{} at indices {i} and {j}",
                        a.from,
                        a.to
                    );
                }
            }
        }
    }

    #[test]
    fn durability_every_transition_has_nonempty_trigger() {
        for t in DURABILITY_TRANSITIONS {
            assert!(
                !t.trigger.is_empty(),
                "transition {}->{} has empty trigger",
                t.from,
                t.to
            );
        }
    }

    #[test]
    fn durability_severity_is_strictly_monotonic() {
        // All 4 states must have distinct severity values (no ties).
        let mut severities: Vec<u8> = ALL_DURABILITY_STATES.iter().map(|s| s.severity()).collect();
        severities.sort_unstable();
        severities.dedup();
        assert_eq!(
            severities.len(),
            4,
            "all 4 states must have distinct severity"
        );
    }

    #[test]
    fn durability_from_mailbox_state_never_loses_severity() {
        // Collapsing 7→4 states must be severity-monotone:
        // if mailbox_a.severity > mailbox_b.severity, then
        // durability(mailbox_a).severity >= durability(mailbox_b).severity.
        let all_mailbox = [
            MailboxState::Healthy,
            MailboxState::Stale,
            MailboxState::Suspect,
            MailboxState::DegradedReadOnly,
            MailboxState::Recovering,
            MailboxState::Broken,
            MailboxState::Escalate,
        ];
        for &a in &all_mailbox {
            for &b in &all_mailbox {
                if a.severity() > b.severity() {
                    let da = DurabilityState::from_mailbox_state(a);
                    let db = DurabilityState::from_mailbox_state(b);
                    assert!(
                        da.severity() >= db.severity(),
                        "severity monotonicity violated: {a}(sev={}) -> {da}(sev={}), \
                         {b}(sev={}) -> {db}(sev={})",
                        a.severity(),
                        da.severity(),
                        b.severity(),
                        db.severity()
                    );
                }
            }
        }
    }

    #[test]
    fn durability_corrupt_requires_recovering_to_reach_healthy() {
        // Corrupt has no direct path to Healthy — must go through Recovering.
        // Verify: the only exit from Corrupt is to Recovering.
        let corrupt_exits: Vec<DurabilityState> = DURABILITY_TRANSITIONS
            .iter()
            .filter(|t| t.from == DurabilityState::Corrupt)
            .map(|t| t.to)
            .collect();
        assert_eq!(corrupt_exits, vec![DurabilityState::Recovering]);

        // Also verify Corrupt→Healthy is explicitly rejected.
        validate_durability_transition(DurabilityState::Corrupt, DurabilityState::Healthy)
            .expect_err("Corrupt→Healthy must be invalid");

        // And Corrupt→DegradedReadOnly is rejected.
        validate_durability_transition(DurabilityState::Corrupt, DurabilityState::DegradedReadOnly)
            .expect_err("Corrupt→DegradedReadOnly must be invalid");
    }

    #[test]
    fn durability_combined_health_and_verdict_max_severity() {
        use mcp_agent_mail_core::HealthLevel;

        // When both a MailboxState verdict and a HealthLevel floor are available,
        // max_severity should produce the most severe of the two.
        let combos: &[(MailboxState, HealthLevel, DurabilityState)] = &[
            // Healthy verdict + Green health → Healthy
            (
                MailboxState::Healthy,
                HealthLevel::Green,
                DurabilityState::Healthy,
            ),
            // Healthy verdict + Red health → DegradedReadOnly (floor wins)
            (
                MailboxState::Healthy,
                HealthLevel::Red,
                DurabilityState::DegradedReadOnly,
            ),
            // Broken verdict + Green health → Corrupt (verdict wins)
            (
                MailboxState::Broken,
                HealthLevel::Green,
                DurabilityState::Corrupt,
            ),
            // Stale verdict + Red health → DegradedReadOnly (both agree)
            (
                MailboxState::Stale,
                HealthLevel::Red,
                DurabilityState::DegradedReadOnly,
            ),
            // Recovering verdict + Yellow health → Recovering (verdict wins)
            (
                MailboxState::Recovering,
                HealthLevel::Yellow,
                DurabilityState::Recovering,
            ),
            // Escalate verdict + Red health → Corrupt (verdict wins, Corrupt > DegradedReadOnly)
            (
                MailboxState::Escalate,
                HealthLevel::Red,
                DurabilityState::Corrupt,
            ),
        ];

        for &(verdict, health, expected) in combos {
            let from_verdict = DurabilityState::from_mailbox_state(verdict);
            let from_health = DurabilityState::floor_from_health_level(health);
            let combined = from_verdict.max_severity(from_health);
            assert_eq!(
                combined, expected,
                "verdict={verdict}, health={health:?}: expected {expected}, got {combined}"
            );
        }
    }

    #[test]
    fn durability_recovery_start_matches_transitions_to_recovering() {
        // States that allow recovery_start should be exactly those that have
        // a transition edge to Recovering in DURABILITY_TRANSITIONS.
        let states_with_recovering_exit: std::collections::HashSet<DurabilityState> =
            DURABILITY_TRANSITIONS
                .iter()
                .filter(|t| t.to == DurabilityState::Recovering)
                .map(|t| t.from)
                .collect();

        for &state in &ALL_DURABILITY_STATES {
            let allows = state.allows_recovery_start();
            let has_edge = states_with_recovering_exit.contains(&state);
            assert_eq!(
                allows, has_edge,
                "{state}: allows_recovery_start()={allows} but has_edge_to_Recovering={has_edge}"
            );
        }
    }

    #[test]
    fn durability_every_state_reachable_from_healthy() {
        // BFS from Healthy should reach all 4 states (the graph is connected).
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(DurabilityState::Healthy);
        visited.insert(DurabilityState::Healthy);

        while let Some(current) = queue.pop_front() {
            for t in DURABILITY_TRANSITIONS {
                if t.from == current && !visited.contains(&t.to) {
                    visited.insert(t.to);
                    queue.push_back(t.to);
                }
            }
        }
        assert_eq!(
            visited.len(),
            4,
            "all states must be reachable from Healthy; unreachable: {:?}",
            ALL_DURABILITY_STATES
                .iter()
                .filter(|s| !visited.contains(s))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn durability_healthy_reachable_from_every_state() {
        // Every state can eventually return to Healthy (BFS in reverse).
        for &start in &ALL_DURABILITY_STATES {
            let mut visited = std::collections::HashSet::new();
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(start);
            visited.insert(start);

            while let Some(current) = queue.pop_front() {
                for t in DURABILITY_TRANSITIONS {
                    if t.from == current && !visited.contains(&t.to) {
                        visited.insert(t.to);
                        queue.push_back(t.to);
                    }
                }
            }
            assert!(
                visited.contains(&DurabilityState::Healthy),
                "Healthy must be reachable from {start}"
            );
        }
    }

    #[test]
    fn durability_as_str_matches_display() {
        for &state in &ALL_DURABILITY_STATES {
            assert_eq!(
                state.as_str(),
                &state.to_string(),
                "{state:?}: as_str() and Display must agree"
            );
        }
    }

    #[test]
    fn durability_max_severity_is_commutative() {
        for &a in &ALL_DURABILITY_STATES {
            for &b in &ALL_DURABILITY_STATES {
                assert_eq!(
                    a.max_severity(b),
                    b.max_severity(a),
                    "max_severity must be commutative: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn durability_max_severity_is_idempotent() {
        for &a in &ALL_DURABILITY_STATES {
            assert_eq!(
                a.max_severity(a),
                a,
                "max_severity with self must be identity: {a}"
            );
        }
    }
}
