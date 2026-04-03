//! Authoritative mailbox durability state machine and invariants (br-97gc6.5.2.1.1).
//!
//! This module defines the single source of truth for mailbox durability
//! semantics across startup, doctor, CLI, and server entrypoints. The goal is
//! to stop each surface from inventing its own meaning for "stale",
//! "corrupt", or "recovering".
//!
//! # State Machine
//!
//! ```text
//! Healthy
//!   ├─ drift/freshness lag ───────────────► Stale
//!   ├─ conflicting or suspicious evidence ► Suspect
//!   ├─ decisive live-path failure ───────► Broken
//!   ├─ safe snapshot available ──────────► DegradedReadOnly
//!   └─ authority/source lost ────────────► Escalate
//!
//! Stale
//!   ├─ parity restored ──────────────────► Healthy
//!   ├─ evidence worsens ─────────────────► Suspect | Broken | DegradedReadOnly
//!   ├─ supervisor starts repair ─────────► Recovering
//!   └─ authority/source lost ────────────► Escalate
//!
//! Suspect
//!   ├─ suspicion cleared ────────────────► Healthy | Stale
//!   ├─ safe snapshot chosen ─────────────► DegradedReadOnly
//!   ├─ decisive failure confirmed ───────► Broken
//!   ├─ supervisor starts repair ─────────► Recovering
//!   └─ ambiguity becomes unsafe ─────────► Escalate
//!
//! Broken
//!   ├─ safe snapshot becomes available ──► DegradedReadOnly
//!   ├─ supervisor starts repair ─────────► Recovering
//!   └─ no safe automatic path remains ───► Escalate
//!
//! DegradedReadOnly
//!   ├─ verified promotion/parity restored► Healthy
//!   ├─ safe snapshot lost ───────────────► Broken
//!   ├─ supervisor starts repair ─────────► Recovering
//!   └─ safety/authority worsens ─────────► Escalate
//!
//! Recovering
//!   ├─ verified candidate promoted ──────► Healthy
//!   ├─ candidate fails, snapshots remain ► DegradedReadOnly
//!   ├─ recovery aborts, no read path ────► Broken
//!   └─ retries exhausted / conflict ─────► Escalate
//!
//! Escalate
//!   ├─ operator authorizes recovery ─────► Recovering
//!   ├─ operator serves snapshots only ───► DegradedReadOnly
//!   └─ operator clears false positive ───► Healthy
//! ```
//!
//! # Authority Boundary
//!
//! - The DB-layer verdict engine classifies evidence into a durability state.
//! - The mailbox supervisor owns exclusive recovery and any supervisor-only
//!   writes while the mailbox is degraded.
//! - The operator owns `Escalate` exit decisions.
//!
//! # Safety Rule
//!
//! Once the mailbox is no longer `Healthy`, no caller may assume direct mutable
//! access to the live SQLite file. All mutating behavior must follow the
//! per-state write policy defined here.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

/// All durability states in severity order from most healthy to least automatic.
pub const MAILBOX_DURABILITY_STATES: &[MailboxDurabilityState] = &[
    MailboxDurabilityState::Healthy,
    MailboxDurabilityState::Stale,
    MailboxDurabilityState::Suspect,
    MailboxDurabilityState::Broken,
    MailboxDurabilityState::DegradedReadOnly,
    MailboxDurabilityState::Recovering,
    MailboxDurabilityState::Escalate,
];

/// Authoritative durability state for a mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxDurabilityState {
    /// SQLite, archive, and ownership signals are mutually consistent.
    Healthy,
    /// The archive remains authoritative, but live indexing/freshness is lagging.
    Stale,
    /// Evidence is conflicting or incomplete, so the live mutation path is no longer trusted.
    Suspect,
    /// No trustworthy live mailbox path is currently available.
    Broken,
    /// Reads may continue from verified archive snapshots while user-facing writes are stopped.
    DegradedReadOnly,
    /// A single supervisor-owned recovery attempt is actively rebuilding or validating a candidate.
    Recovering,
    /// Automatic recovery is unsafe or ambiguous; explicit operator action is required.
    Escalate,
}

impl MailboxDurabilityState {
    /// Static contract for this state.
    #[must_use]
    pub const fn contract(self) -> &'static MailboxDurabilityContract {
        match self {
            Self::Healthy => &HEALTHY_CONTRACT,
            Self::Stale => &STALE_CONTRACT,
            Self::Suspect => &SUSPECT_CONTRACT,
            Self::Broken => &BROKEN_CONTRACT,
            Self::DegradedReadOnly => &DEGRADED_READ_ONLY_CONTRACT,
            Self::Recovering => &RECOVERING_CONTRACT,
            Self::Escalate => &ESCALATE_CONTRACT,
        }
    }

    /// Read policy for this state.
    #[must_use]
    pub const fn read_policy(self) -> MailboxReadPolicy {
        self.contract().read_policy
    }

    /// Write policy for this state.
    #[must_use]
    pub const fn write_policy(self) -> MailboxWritePolicy {
        self.contract().write_policy
    }

    /// Recovery requirement for this state.
    #[must_use]
    pub const fn recovery_requirement(self) -> MailboxRecoveryRequirement {
        self.contract().recovery_requirement
    }

    /// Which layer owns progression out of this state.
    #[must_use]
    pub const fn transition_authority(self) -> MailboxTransitionAuthority {
        self.contract().transition_authority
    }

    /// Whether any safe read surface may continue in this state.
    #[must_use]
    pub const fn reads_may_continue(self) -> bool {
        !matches!(self.read_policy(), MailboxReadPolicy::HoldAll)
    }

    /// Whether normal user-facing writes must stop in this state.
    #[must_use]
    pub const fn writes_must_stop(self) -> bool {
        matches!(
            self.write_policy(),
            MailboxWritePolicy::SupervisorOnly | MailboxWritePolicy::Blocked
        )
    }

    /// Whether this state is degraded relative to normal operation.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        !matches!(self, Self::Healthy)
    }
}

impl std::fmt::Display for MailboxDurabilityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::Stale => f.write_str("stale"),
            Self::Suspect => f.write_str("suspect"),
            Self::Broken => f.write_str("broken"),
            Self::DegradedReadOnly => f.write_str("degraded_read_only"),
            Self::Recovering => f.write_str("recovering"),
            Self::Escalate => f.write_str("escalate"),
        }
    }
}

/// Read contract for a durability state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxReadPolicy {
    /// Live SQLite reads are preferred.
    LiveDbPreferred,
    /// Verified archive snapshots are preferred; bounded live fallback needs explicit degraded handling.
    ArchiveSnapshotPreferred,
    /// Reads must be served from a verified snapshot/candidate, never the raw live DB path.
    ArchiveSnapshotRequired,
    /// No safe reads may continue automatically.
    HoldAll,
}

/// Write contract for a durability state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxWritePolicy {
    /// Normal mutating entrypoints may proceed.
    LiveMutationAllowed,
    /// Writes must be routed through the single mailbox owner/broker, not peer-mutated directly.
    OwnerBrokerOnly,
    /// Only supervisor-owned recovery receipts/checkpoints may mutate state.
    SupervisorOnly,
    /// No automatic writes are allowed.
    Blocked,
}

/// How strongly the state demands exclusive recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxRecoveryRequirement {
    /// No exclusive recovery is needed.
    None,
    /// Recovery may be scheduled proactively, but is not yet mandatory.
    Optional,
    /// Exclusive recovery is required before normal writes can resume.
    Required,
    /// Automatic recovery is insufficient; an operator must intervene first.
    OperatorRequired,
}

/// Who owns transitions out of the current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxTransitionAuthority {
    /// The centralized verdict engine can classify entry/exit based on evidence.
    VerdictEngine,
    /// The single-flight mailbox supervisor owns progress while recovery is active.
    MailboxSupervisor,
    /// A human/operator must make the next decision.
    Operator,
}

/// Per-state contract consumed by downstream durability work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MailboxDurabilityContract {
    /// State this contract describes.
    pub state: MailboxDurabilityState,
    /// Human-readable summary of the state.
    pub summary: &'static str,
    /// Evidence threshold that places the mailbox in this state.
    pub entry_evidence: &'static str,
    /// Safe read policy while in this state.
    pub read_policy: MailboxReadPolicy,
    /// Safe write policy while in this state.
    pub write_policy: MailboxWritePolicy,
    /// Whether exclusive recovery is needed.
    pub recovery_requirement: MailboxRecoveryRequirement,
    /// Which layer owns progression out of the state.
    pub transition_authority: MailboxTransitionAuthority,
}

const HEALTHY_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Healthy,
    summary: "SQLite, archive, and ownership evidence are mutually consistent.",
    entry_evidence: "Integrity, parity, freshness, and ownership probes all pass within policy budgets.",
    read_policy: MailboxReadPolicy::LiveDbPreferred,
    write_policy: MailboxWritePolicy::LiveMutationAllowed,
    recovery_requirement: MailboxRecoveryRequirement::None,
    transition_authority: MailboxTransitionAuthority::VerdictEngine,
};

const STALE_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Stale,
    summary: "Archive authority is intact, but live indexing or freshness is behind.",
    entry_evidence: "Archive-vs-DB parity, freshness, or lag budgets fail without corruption or owner ambiguity.",
    read_policy: MailboxReadPolicy::ArchiveSnapshotPreferred,
    write_policy: MailboxWritePolicy::OwnerBrokerOnly,
    recovery_requirement: MailboxRecoveryRequirement::Optional,
    transition_authority: MailboxTransitionAuthority::VerdictEngine,
};

const SUSPECT_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Suspect,
    summary: "Evidence is conflicting or incomplete, so the live mutation path is no longer trusted.",
    entry_evidence: "Live sidecars, retryable corruption signatures, snapshot conflicts, owner-liveness anomalies, or unresolved ownership hints are present.",
    read_policy: MailboxReadPolicy::ArchiveSnapshotPreferred,
    write_policy: MailboxWritePolicy::OwnerBrokerOnly,
    recovery_requirement: MailboxRecoveryRequirement::Optional,
    transition_authority: MailboxTransitionAuthority::VerdictEngine,
};

const BROKEN_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Broken,
    summary: "The mailbox has no trustworthy live service path.",
    entry_evidence: "Decisive corruption, missing primary state, or failed health probes leave no safe live DB path and no verified read fallback is active yet.",
    read_policy: MailboxReadPolicy::HoldAll,
    write_policy: MailboxWritePolicy::Blocked,
    recovery_requirement: MailboxRecoveryRequirement::Required,
    transition_authority: MailboxTransitionAuthority::VerdictEngine,
};

const DEGRADED_READ_ONLY_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::DegradedReadOnly,
    summary: "Reads may continue from verified archive snapshots while normal writes are stopped.",
    entry_evidence: "Archive authority remains readable and a safe snapshot/candidate path exists, but live mutation is unsafe.",
    read_policy: MailboxReadPolicy::ArchiveSnapshotRequired,
    write_policy: MailboxWritePolicy::SupervisorOnly,
    recovery_requirement: MailboxRecoveryRequirement::Required,
    transition_authority: MailboxTransitionAuthority::VerdictEngine,
};

const RECOVERING_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Recovering,
    summary: "A single supervisor-owned recovery attempt is rebuilding or validating a candidate.",
    entry_evidence: "An exclusive recovery owner has fenced writes and started candidate build, replay, or promotion validation.",
    read_policy: MailboxReadPolicy::ArchiveSnapshotRequired,
    write_policy: MailboxWritePolicy::SupervisorOnly,
    recovery_requirement: MailboxRecoveryRequirement::Required,
    transition_authority: MailboxTransitionAuthority::MailboxSupervisor,
};

const ESCALATE_CONTRACT: MailboxDurabilityContract = MailboxDurabilityContract {
    state: MailboxDurabilityState::Escalate,
    summary: "Automatic recovery is unsafe or ambiguous; operator action is required.",
    entry_evidence: "Archive authority is unreadable/inconsistent, ownership is split-brain or missing, or repeated recovery attempts fail closed.",
    read_policy: MailboxReadPolicy::HoldAll,
    write_policy: MailboxWritePolicy::Blocked,
    recovery_requirement: MailboxRecoveryRequirement::OperatorRequired,
    transition_authority: MailboxTransitionAuthority::Operator,
};

/// All per-state contracts.
pub const MAILBOX_DURABILITY_CONTRACTS: &[MailboxDurabilityContract] = &[
    HEALTHY_CONTRACT,
    STALE_CONTRACT,
    SUSPECT_CONTRACT,
    BROKEN_CONTRACT,
    DEGRADED_READ_ONLY_CONTRACT,
    RECOVERING_CONTRACT,
    ESCALATE_CONTRACT,
];

/// A single allowed durability transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MailboxDurabilityTransition {
    /// Source state.
    pub from: MailboxDurabilityState,
    /// Destination state.
    pub to: MailboxDurabilityState,
    /// Short trigger for the transition.
    pub trigger: &'static str,
    /// Why the transition is allowed.
    pub rationale: &'static str,
}

/// All allowed transitions between durability states.
pub const MAILBOX_DURABILITY_TRANSITIONS: &[MailboxDurabilityTransition] = &[
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Healthy,
        to: MailboxDurabilityState::Stale,
        trigger: "parity_or_freshness_lag",
        rationale: "The archive is still authoritative, but the live view is lagging.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Healthy,
        to: MailboxDurabilityState::Suspect,
        trigger: "conflicting_evidence",
        rationale: "Suspicious evidence exists, but decisive failure has not been proven.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Healthy,
        to: MailboxDurabilityState::Broken,
        trigger: "decisive_live_path_failure",
        rationale: "The live mailbox path is unsafe and no verified read fallback is active.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Healthy,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "safe_snapshot_available_after_live_failure",
        rationale: "The live path is unsafe, but reads can continue from a verified snapshot.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Healthy,
        to: MailboxDurabilityState::Escalate,
        trigger: "authority_or_archive_lost",
        rationale: "The system cannot safely decide on recovery without operator help.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::Healthy,
        trigger: "parity_restored",
        rationale: "Lag is cleared and all health budgets pass again.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::Suspect,
        trigger: "new_suspicion",
        rationale: "Staleness has progressed into conflicting or unsafe evidence.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::Broken,
        trigger: "live_path_breaks_without_snapshot",
        rationale: "Stale state worsened and no safe read fallback is active.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "switch_to_verified_snapshot_reads",
        rationale: "Reads move off the live path while repairs are planned or pending.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::Recovering,
        trigger: "exclusive_recovery_started",
        rationale: "The supervisor proactively began a single-flight repair/rebuild.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Stale,
        to: MailboxDurabilityState::Escalate,
        trigger: "authority_or_archive_lost",
        rationale: "Stale mode can no longer be handled automatically.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::Healthy,
        trigger: "suspicion_cleared",
        rationale: "Suspicious evidence proved transient and healthy signals now agree.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::Stale,
        trigger: "suspicion_cleared_but_lag_remains",
        rationale: "Unsafe evidence cleared, but the archive is still ahead or fresher.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::Broken,
        trigger: "failure_confirmed",
        rationale: "Investigation converted suspicion into decisive failure.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "snapshot_path_chosen",
        rationale: "A verified snapshot path is available, so reads can continue safely.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::Recovering,
        trigger: "exclusive_recovery_started",
        rationale: "The supervisor fenced writers and began repair from a suspect state.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Suspect,
        to: MailboxDurabilityState::Escalate,
        trigger: "unsafe_ambiguity",
        rationale: "The system cannot resolve conflicting signals automatically.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Broken,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "verified_snapshot_available",
        rationale: "Read service can resume safely before the mailbox becomes writable again.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Broken,
        to: MailboxDurabilityState::Recovering,
        trigger: "exclusive_recovery_started",
        rationale: "The supervisor admitted an exclusive repair path for a broken mailbox.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Broken,
        to: MailboxDurabilityState::Escalate,
        trigger: "no_safe_automatic_path",
        rationale: "Broken state cannot be repaired safely without operator action.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::DegradedReadOnly,
        to: MailboxDurabilityState::Healthy,
        trigger: "verified_promotion",
        rationale: "Parity and authority have been restored by a verified promotion or replay.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::DegradedReadOnly,
        to: MailboxDurabilityState::Broken,
        trigger: "snapshot_lost",
        rationale: "The safe read fallback disappeared before recovery completed.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::DegradedReadOnly,
        to: MailboxDurabilityState::Recovering,
        trigger: "exclusive_recovery_started",
        rationale: "Degraded reads continue while an exclusive recovery attempt runs.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::DegradedReadOnly,
        to: MailboxDurabilityState::Escalate,
        trigger: "safety_or_authority_worsened",
        rationale: "Even snapshot-only service is no longer automatically safe.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Recovering,
        to: MailboxDurabilityState::Healthy,
        trigger: "candidate_promoted",
        rationale: "A rebuilt or repaired candidate passed verification and was promoted.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Recovering,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "candidate_failed_but_snapshots_remain",
        rationale: "Recovery failed closed, but safe read-only service can continue.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Recovering,
        to: MailboxDurabilityState::Broken,
        trigger: "recovery_aborted_without_snapshot",
        rationale: "Recovery stopped and no safe read surface remains available.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Recovering,
        to: MailboxDurabilityState::Escalate,
        trigger: "retries_exhausted_or_conflict_detected",
        rationale: "Automatic recovery must stop and hand off to an operator.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Escalate,
        to: MailboxDurabilityState::Recovering,
        trigger: "operator_authorized_recovery",
        rationale: "A human/operator explicitly approved the next recovery attempt.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Escalate,
        to: MailboxDurabilityState::DegradedReadOnly,
        trigger: "operator_chose_snapshot_only_service",
        rationale: "The operator elected to keep reads available while holding writes closed.",
    },
    MailboxDurabilityTransition {
        from: MailboxDurabilityState::Escalate,
        to: MailboxDurabilityState::Healthy,
        trigger: "operator_cleared_false_positive",
        rationale: "An operator restored authority or proved the failure signal was false.",
    },
];

/// Validate a durability transition.
pub fn validate_mailbox_durability_transition(
    from: MailboxDurabilityState,
    to: MailboxDurabilityState,
) -> Result<(), &'static str> {
    if from == to {
        return Ok(());
    }
    if MAILBOX_DURABILITY_TRANSITIONS
        .iter()
        .any(|transition| transition.from == from && transition.to == to)
    {
        Ok(())
    } else {
        Err("invalid mailbox durability transition")
    }
}

/// A non-negotiable mailbox durability invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MailboxDurabilityInvariant {
    /// Stable identifier for the invariant.
    pub id: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Property the system must uphold.
    pub property: &'static str,
    /// What breaks if the invariant is violated.
    pub violation_consequence: &'static str,
    /// How to verify the invariant.
    pub verification: &'static str,
}

/// Mailbox durability invariants required by the supervisor epic.
pub const MAILBOX_DURABILITY_INVARIANTS: &[MailboxDurabilityInvariant] = &[
    MailboxDurabilityInvariant {
        id: "inv.single_verdict_authority",
        description: "Mailbox durability state classification is centralized",
        property: "CLI, server, startup, and doctor surfaces all consume the same DB-layer durability verdict instead of re-deriving local semantics.",
        violation_consequence: "Different entrypoints disagree on whether the mailbox is writable, recoverable, or safe to read.",
        verification: "Unit/integration tests assert all entrypoints use the same verdict matrix and transition validator.",
    },
    MailboxDurabilityInvariant {
        id: "inv.no_peer_mutation_when_degraded",
        description: "Degraded mailboxes forbid peer-direct mutation",
        property: "In any state other than Healthy, direct live-SQLite writes by arbitrary callers are forbidden; only owner-brokered or supervisor-only paths may mutate state.",
        violation_consequence: "Split-brain writes, recovery races, and archive/DB divergence reappear under load or restart.",
        verification: "State-policy tests plus E2E contention tests verify write admission obeys the per-state write policy.",
    },
    MailboxDurabilityInvariant {
        id: "inv.read_only_requires_verified_snapshot",
        description: "Read-only degradation never serves unverified live state",
        property: "DegradedReadOnly and Recovering reads come from a verified archive snapshot or validated candidate, never from an untrusted live DB path.",
        violation_consequence: "Users observe stale or corrupted data while the system falsely claims read-only safety.",
        verification: "Fault-injection tests confirm degraded reads switch to verified snapshot sources only.",
    },
    MailboxDurabilityInvariant {
        id: "inv.single_flight_recovery",
        description: "Only one exclusive recovery owner may act at a time",
        property: "At most one recovery owner/candidate/promotion path may hold the mailbox in Recovering for a given mailbox identity.",
        violation_consequence: "Concurrent recovery attempts can overwrite each other, orphan artifacts, or oscillate the verdict.",
        verification: "State-machine and mailbox-lock tests verify second recovery attempts are rejected or queued.",
    },
    MailboxDurabilityInvariant {
        id: "inv_no_fresh_start_over_quarantine",
        description: "Quarantined artifacts block blank reinitialization",
        property: "If quarantine or recovery artifacts exist, the mailbox may not silently treat missing primary state as a fresh start.",
        violation_consequence: "Operators lose forensic evidence and DB-only state by silently rebuilding over preserved artifacts.",
        verification: "Unit tests cover missing-primary + quarantine-artifact combinations and require fail-closed behavior.",
    },
    MailboxDurabilityInvariant {
        id: "inv_recovery_promotion_is_monotone",
        description: "Only verified promotion restores writable health",
        property: "Broken, DegradedReadOnly, and Recovering may return to Healthy only after verification/promotion evidence, never by local optimism or retry success alone.",
        violation_consequence: "The mailbox re-enters writable service before archive parity, candidate health, or authority safety is proven.",
        verification: "Transition-matrix tests reject direct unwarranted jumps back to Healthy.",
    },
    MailboxDurabilityInvariant {
        id: "inv_escalate_requires_operator_exit",
        description: "Escalate cannot auto-clear",
        property: "Escalate exits only via explicit operator-authorized transitions.",
        violation_consequence: "The system resumes mutation after unrecoverable or ambiguous failures without human review.",
        verification: "Transition tests and CLI/server integration paths require operator-driven exit triggers from Escalate.",
    },
];

/// Look up a mailbox durability invariant by ID.
#[must_use]
pub fn mailbox_durability_invariant_by_id(id: &str) -> Option<&'static MailboxDurabilityInvariant> {
    MAILBOX_DURABILITY_INVARIANTS
        .iter()
        .find(|invariant| invariant.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_state_has_exactly_one_contract() {
        let mut seen = HashSet::new();
        for contract in MAILBOX_DURABILITY_CONTRACTS {
            assert!(
                seen.insert(contract.state),
                "duplicate contract for state {}",
                contract.state
            );
        }
        assert_eq!(
            seen.len(),
            MAILBOX_DURABILITY_STATES.len(),
            "every durability state must have one contract"
        );
    }

    #[test]
    fn display_matches_expected_state_names() {
        assert_eq!(MailboxDurabilityState::Healthy.to_string(), "healthy");
        assert_eq!(MailboxDurabilityState::Stale.to_string(), "stale");
        assert_eq!(MailboxDurabilityState::Suspect.to_string(), "suspect");
        assert_eq!(MailboxDurabilityState::Broken.to_string(), "broken");
        assert_eq!(
            MailboxDurabilityState::DegradedReadOnly.to_string(),
            "degraded_read_only"
        );
        assert_eq!(MailboxDurabilityState::Recovering.to_string(), "recovering");
        assert_eq!(MailboxDurabilityState::Escalate.to_string(), "escalate");
    }

    #[test]
    fn same_state_transitions_are_idempotent() {
        for &state in MAILBOX_DURABILITY_STATES {
            validate_mailbox_durability_transition(state, state)
                .expect("same-state transition should be allowed");
        }
    }

    #[test]
    fn transition_matrix_allows_expected_edges() {
        for &(from, to) in &[
            (
                MailboxDurabilityState::Healthy,
                MailboxDurabilityState::Stale,
            ),
            (
                MailboxDurabilityState::Stale,
                MailboxDurabilityState::Recovering,
            ),
            (
                MailboxDurabilityState::Suspect,
                MailboxDurabilityState::DegradedReadOnly,
            ),
            (
                MailboxDurabilityState::Broken,
                MailboxDurabilityState::Recovering,
            ),
            (
                MailboxDurabilityState::Recovering,
                MailboxDurabilityState::Healthy,
            ),
            (
                MailboxDurabilityState::Escalate,
                MailboxDurabilityState::Recovering,
            ),
        ] {
            validate_mailbox_durability_transition(from, to)
                .unwrap_or_else(|err| panic!("expected {from} -> {to} to be valid: {err}"));
        }
    }

    #[test]
    fn transition_matrix_rejects_invalid_edges() {
        for &(from, to) in &[
            (
                MailboxDurabilityState::Broken,
                MailboxDurabilityState::Healthy,
            ),
            (
                MailboxDurabilityState::Healthy,
                MailboxDurabilityState::Recovering,
            ),
            (
                MailboxDurabilityState::Escalate,
                MailboxDurabilityState::Suspect,
            ),
            (
                MailboxDurabilityState::DegradedReadOnly,
                MailboxDurabilityState::Stale,
            ),
        ] {
            let err = validate_mailbox_durability_transition(from, to)
                .expect_err("unexpected valid transition");
            assert_eq!(err, "invalid mailbox durability transition");
        }
    }

    #[test]
    fn state_policies_capture_read_write_contract() {
        assert_eq!(
            MailboxDurabilityState::Healthy.read_policy(),
            MailboxReadPolicy::LiveDbPreferred
        );
        assert_eq!(
            MailboxDurabilityState::Healthy.write_policy(),
            MailboxWritePolicy::LiveMutationAllowed
        );
        assert!(!MailboxDurabilityState::Healthy.writes_must_stop());

        assert_eq!(
            MailboxDurabilityState::DegradedReadOnly.read_policy(),
            MailboxReadPolicy::ArchiveSnapshotRequired
        );
        assert_eq!(
            MailboxDurabilityState::DegradedReadOnly.write_policy(),
            MailboxWritePolicy::SupervisorOnly
        );
        assert!(MailboxDurabilityState::DegradedReadOnly.reads_may_continue());
        assert!(MailboxDurabilityState::DegradedReadOnly.writes_must_stop());

        assert_eq!(
            MailboxDurabilityState::Broken.read_policy(),
            MailboxReadPolicy::HoldAll
        );
        assert!(!MailboxDurabilityState::Broken.reads_may_continue());
        assert!(MailboxDurabilityState::Broken.writes_must_stop());
    }

    #[test]
    fn recovery_requirement_tracks_state_severity() {
        assert_eq!(
            MailboxDurabilityState::Healthy.recovery_requirement(),
            MailboxRecoveryRequirement::None
        );
        assert_eq!(
            MailboxDurabilityState::Stale.recovery_requirement(),
            MailboxRecoveryRequirement::Optional
        );
        assert_eq!(
            MailboxDurabilityState::Recovering.recovery_requirement(),
            MailboxRecoveryRequirement::Required
        );
        assert_eq!(
            MailboxDurabilityState::Escalate.recovery_requirement(),
            MailboxRecoveryRequirement::OperatorRequired
        );
    }

    #[test]
    fn transition_authority_matches_expected_owner() {
        assert_eq!(
            MailboxDurabilityState::Healthy.transition_authority(),
            MailboxTransitionAuthority::VerdictEngine
        );
        assert_eq!(
            MailboxDurabilityState::Recovering.transition_authority(),
            MailboxTransitionAuthority::MailboxSupervisor
        );
        assert_eq!(
            MailboxDurabilityState::Escalate.transition_authority(),
            MailboxTransitionAuthority::Operator
        );
    }

    #[test]
    fn invariant_ids_are_unique_and_resolvable() {
        let mut ids = HashSet::new();
        for invariant in MAILBOX_DURABILITY_INVARIANTS {
            assert!(
                ids.insert(invariant.id),
                "duplicate invariant id {}",
                invariant.id
            );
            assert_eq!(
                mailbox_durability_invariant_by_id(invariant.id),
                Some(invariant)
            );
        }
    }

    // ── Cross-surface golden snapshot tests (br-97gc6.5.2.6.4.1) ────

    /// The five canonical operator/user surfaces.
    const SURFACES: &[&str] = &["cli", "server", "web", "robot", "tui"];

    /// A snapshot of the durability contract as seen from a single surface.
    ///
    /// Every surface MUST derive the same values from the authoritative
    /// `MailboxDurabilityContract`.  If any surface ever needs to diverge,
    /// it must be captured as an explicit exception with a rationale.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
    struct SurfaceSnapshot {
        surface: String,
        state: String,
        summary: String,
        read_policy: String,
        write_policy: String,
        recovery_requirement: String,
        transition_authority: String,
        reads_may_continue: bool,
        writes_must_stop: bool,
        is_degraded: bool,
    }

    /// Build the canonical snapshot for a (surface, state) pair.
    ///
    /// Today every surface derives identical policy from the contract.  If a
    /// surface ever adds surface-specific copy (e.g. a TUI-only "press R to
    /// retry" hint), add it as an optional field rather than changing the
    /// shared fields, so the shared assertions keep catching drift.
    fn surface_snapshot(surface: &str, state: MailboxDurabilityState) -> SurfaceSnapshot {
        let contract = state.contract();
        SurfaceSnapshot {
            surface: surface.to_string(),
            state: state.to_string(),
            summary: contract.summary.to_string(),
            read_policy: format!("{:?}", contract.read_policy),
            write_policy: format!("{:?}", contract.write_policy),
            recovery_requirement: format!("{:?}", contract.recovery_requirement),
            transition_authority: format!("{:?}", contract.transition_authority),
            reads_may_continue: state.reads_may_continue(),
            writes_must_stop: state.writes_must_stop(),
            is_degraded: state.is_degraded(),
        }
    }

    #[test]
    fn all_surfaces_agree_on_every_durability_state() {
        for &state in MAILBOX_DURABILITY_STATES {
            let snapshots: Vec<SurfaceSnapshot> =
                SURFACES.iter().map(|s| surface_snapshot(s, state)).collect();

            // Compare every surface against the first (cli).
            let reference = &snapshots[0];
            for snap in &snapshots[1..] {
                assert_eq!(
                    snap.summary, reference.summary,
                    "summary mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.read_policy, reference.read_policy,
                    "read_policy mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.write_policy, reference.write_policy,
                    "write_policy mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.recovery_requirement, reference.recovery_requirement,
                    "recovery_requirement mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.transition_authority, reference.transition_authority,
                    "transition_authority mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.reads_may_continue, reference.reads_may_continue,
                    "reads_may_continue mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.writes_must_stop, reference.writes_must_stop,
                    "writes_must_stop mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
                assert_eq!(
                    snap.is_degraded, reference.is_degraded,
                    "is_degraded mismatch: {} vs {} in state {}",
                    snap.surface, reference.surface, state
                );
            }
        }
    }

    #[test]
    fn golden_matrix_is_complete_and_stable() {
        // Build the full 5×7 = 35 snapshot matrix.
        let mut matrix: Vec<SurfaceSnapshot> = Vec::with_capacity(35);
        for &state in MAILBOX_DURABILITY_STATES {
            for surface in SURFACES {
                matrix.push(surface_snapshot(surface, state));
            }
        }
        assert_eq!(matrix.len(), SURFACES.len() * MAILBOX_DURABILITY_STATES.len());

        // Verify the matrix serializes to stable JSON (regression gate).
        let json = serde_json::to_string_pretty(&matrix).expect("serialize matrix");
        assert!(json.contains("\"healthy\""), "matrix must contain healthy state");
        assert!(json.contains("\"escalate\""), "matrix must contain escalate state");
        assert!(json.contains("\"tui\""), "matrix must contain tui surface");
        assert!(json.contains("\"robot\""), "matrix must contain robot surface");
    }

    #[test]
    fn golden_snapshot_policy_invariants() {
        // Encode the non-negotiable policy constraints that every surface must obey.
        for &state in MAILBOX_DURABILITY_STATES {
            let snap = surface_snapshot("cli", state);
            match state {
                MailboxDurabilityState::Healthy => {
                    assert!(!snap.writes_must_stop, "healthy must allow writes");
                    assert!(snap.reads_may_continue, "healthy must allow reads");
                    assert!(!snap.is_degraded, "healthy must not be degraded");
                }
                MailboxDurabilityState::Broken => {
                    assert!(snap.writes_must_stop, "broken must stop writes");
                    assert!(!snap.reads_may_continue, "broken must hold reads");
                    assert!(snap.is_degraded);
                }
                MailboxDurabilityState::Escalate => {
                    assert!(snap.writes_must_stop, "escalate must stop writes");
                    assert!(!snap.reads_may_continue, "escalate must hold reads");
                    assert_eq!(snap.transition_authority, "Operator");
                    assert_eq!(snap.recovery_requirement, "OperatorRequired");
                }
                MailboxDurabilityState::DegradedReadOnly => {
                    assert!(snap.writes_must_stop, "degraded_read_only must stop writes");
                    assert!(snap.reads_may_continue, "degraded_read_only must allow reads");
                    assert_eq!(snap.read_policy, "ArchiveSnapshotRequired");
                }
                MailboxDurabilityState::Recovering => {
                    assert!(snap.writes_must_stop, "recovering must stop user writes");
                    assert!(snap.reads_may_continue, "recovering must allow snapshot reads");
                    assert_eq!(snap.transition_authority, "MailboxSupervisor");
                }
                _ => {
                    assert!(snap.is_degraded, "{state} must be degraded");
                }
            }
        }
    }

    #[test]
    fn golden_snapshot_write_file_on_update() {
        // Emit the golden snapshot matrix to a well-known path for CI diffing.
        // This test always passes — it just writes the current truth.
        let mut matrix: Vec<SurfaceSnapshot> = Vec::new();
        for &state in MAILBOX_DURABILITY_STATES {
            for surface in SURFACES {
                matrix.push(surface_snapshot(surface, state));
            }
        }
        let json = serde_json::to_string_pretty(&matrix).expect("serialize");
        let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/golden_snapshots/durability_surface_matrix.json");
        if let Some(parent) = golden_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&golden_path, json.as_bytes()).unwrap_or_else(|err| {
            eprintln!(
                "warning: could not write golden snapshot to {}: {err}",
                golden_path.display()
            );
        });
    }
}
