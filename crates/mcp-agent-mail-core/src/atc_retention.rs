//! Retention, compaction, and archive policy for ATC learning (`br-0qt6e.1.4`).
//!
//! This module is the canonical storage-lifecycle contract for the ATC
//! learning stack. It defines:
//!
//! - what stays hot in SQLite or the evidence ledger
//! - what is compacted into bounded rollups or batched audit summaries
//! - what is promoted into the Git archive
//! - what explicitly does **not** get archived by default
//! - what metadata must survive compaction so replay and forensics remain
//!   comparable over time
//!
//! # Temperature Model
//!
//! - **Hot**: full-fidelity rows or ledger entries used by live resolution and
//!   near-term debugging.
//! - **Warm**: compacted/derived state that still powers analysis without
//!   keeping raw telemetry forever.
//! - **Cold archive**: low-write, human-auditable artifacts stored in Git.
//! - **Dropped**: high-volume telemetry exhaust removed after its learning and
//!   forensic value has been preserved elsewhere.
//!
//! # Design Rules
//!
//! 1. Raw ATC experience history lives in SQLite, not Git.
//! 2. Open experiences are never compacted away while still unresolved.
//! 3. Rollups preserve learning signal after raw rows age out.
//! 4. Git stores explainability artifacts, not per-tick telemetry exhaust.
//! 5. Any archived artifact must remain comparable to a baseline, prior policy,
//!    or replay scenario without consulting chat history.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

/// Open experiences older than this must become operator-visible as stale work.
pub const OPEN_EXPERIENCE_STALE_AFTER_DAYS: u16 = 7;

/// Open experiences should be terminalized or explicitly put on forensic hold
/// by this age; they must not silently linger forever.
pub const OPEN_EXPERIENCE_TERMINALIZE_AFTER_DAYS: u16 = 30;

/// Full-fidelity resolved raw rows stay queryable at row granularity for this
/// long before compaction becomes the default access path.
pub const RESOLVED_EXPERIENCE_FULL_FIDELITY_DAYS: u16 = 30;

/// Resolved raw rows may be kept in compacted SQLite form until this age before
/// the raw rows themselves are intentionally dropped.
pub const RESOLVED_EXPERIENCE_DROP_AFTER_DAYS: u16 = 365;

/// Materialized rollups should remain available in SQLite for at least this
/// long even after raw rows have been compacted away.
pub const ROLLUP_LIVE_DAYS: u16 = 730;

/// Evidence-ledger detail is a debugging aid, not the canonical long-term
/// archive, so it stays hot only briefly.
pub const EVIDENCE_LEDGER_HOT_DAYS: u16 = 7;

/// Evidence-ledger JSONL tails may be dropped after this many days once their
/// material decisions have been summarized elsewhere.
pub const EVIDENCE_LEDGER_DROP_AFTER_DAYS: u16 = 30;

/// Regimes with no supporting evidence newer than this are stale and must be
/// marked as historical rather than live.
pub const STALE_REGIME_AFTER_DAYS: u16 = 14;

/// Policy snapshots should remain easy to find in live surfaces for this long
/// even though the archive copy remains durable.
pub const POLICY_SNAPSHOT_HOT_DAYS: u16 = 180;

/// Human-readable audit summaries should be emitted no more frequently than
/// this cadence absent a material event.
pub const PERIODIC_AUDIT_CADENCE_DAYS: u16 = 7;

/// Archived artifacts that matter for replay, audit, and verification must
/// remain discoverable for at least two years.
pub const ARCHIVE_DISCOVERABILITY_MIN_DAYS: u16 = 730;

/// Selected scenario-linked forensic traces follow the same minimum
/// discoverability window as other cold-path audit artifacts.
pub const FORENSIC_TRACE_DISCOVERABILITY_DAYS: u16 = ARCHIVE_DISCOVERABILITY_MIN_DAYS;

/// ATC learning artifact categories with distinct retention semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningArtifactKind {
    /// Raw unresolved experience rows in SQLite.
    OpenExperienceRows,
    /// Raw resolved/censored/expired rows in SQLite.
    ResolvedExperienceRows,
    /// Compact per-stratum materialized statistics in SQLite.
    ExperienceRollups,
    /// Low-level evidence-ledger JSONL entries / in-memory tails.
    EvidenceLedgerEntries,
    /// Policy-as-data bundle snapshots used for promotion/rollback/replay.
    PolicyBundleSnapshots,
    /// Machine-readable summaries of regime transitions.
    RegimeSummaries,
    /// Per-change reasoning cards kept on the warm path and folded into
    /// higher-level audit summaries.
    TransparencyCards,
    /// Human-readable or compact machine-readable audit bundles intended for
    /// operator discovery in Git.
    AuditSummaries,
    /// Selected scenario-linked exemplar traces and replay manifests.
    ExemplarTraces,
}

/// Where the canonical live copy of an artifact type resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePlane {
    /// Full-fidelity SQLite tables on the hot path.
    LiveSqlite,
    /// Compact SQLite tables holding derived sufficient statistics.
    CompactedSqlite,
    /// Evidence-ledger JSONL or ring-buffer storage.
    EvidenceLedger,
    /// Git-backed cold archive for explainability artifacts.
    GitArchive,
}

/// How an artifact is reduced over time to control write amplification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// Keep rows raw until they reach a terminal state.
    KeepRawUntilTerminal,
    /// Keep raw rows briefly, then preserve learning value via rollups plus
    /// selected exemplar promotion.
    RollUpAndSelectExemplars,
    /// Maintain compact sufficient statistics directly; no Git mirror by
    /// default.
    IncrementalRollupOnly,
    /// Keep a short debugging tail only.
    BoundedDebugTail,
    /// Immutable policy snapshot artifact.
    ImmutableSnapshot,
    /// Immutable regime summary artifact.
    ImmutableSummary,
    /// Fold detailed cards into periodic/event-driven audit objects.
    BatchIntoAuditSummaries,
    /// Emit selected exemplar traces only when they materially aid forensics.
    PromoteSelectedForensics,
}

/// When an artifact should be promoted into the Git archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveTrigger {
    /// Never archive this artifact type by default.
    Never,
    /// Archive when a policy is promoted, rolled back, or otherwise materially
    /// changed.
    OnPolicyPromotionOrRollback,
    /// Archive when a regime transition becomes material to operator trust.
    OnRegimeChange,
    /// Archive on a bounded cadence for operator-friendly audit history.
    PeriodicAudit,
    /// Archive only when a trace is explicitly promoted as a forensic exemplar.
    SelectedForensicPromotion,
}

/// How long the archive copy must remain discoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveRetention {
    /// No archive copy is required.
    Never,
    /// Keep discoverable for at least N days.
    MinimumDays(u16),
    /// Treat as a durable permanent artifact.
    Indefinite,
}

impl ArchiveRetention {
    /// Whether the artifact is archived at all.
    #[must_use]
    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Never)
    }

    /// Minimum discoverability window in days, if any.
    #[must_use]
    pub const fn minimum_days(self) -> Option<u16> {
        match self {
            Self::MinimumDays(days) => Some(days),
            Self::Never | Self::Indefinite => None,
        }
    }
}

/// What anchor a future verifier/operator uses to compare an artifact to past
/// runs after compaction or archival.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparabilityAnchor {
    /// No archive-side comparison contract is required.
    None,
    /// Compare via `(policy_id, stratum_key, covered_time_window)`.
    PolicyEpochStratumWindow,
    /// Compare via `(policy_bundle_id, comparator_or_baseline_id, covered_window)`.
    PolicySnapshotComparator,
    /// Compare via `(scenario_id, replay_input_hash, comparator_or_baseline_id)`.
    ScenarioReplayComparator,
}

/// One canonical retention rule for one ATC artifact class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactRetentionRule {
    /// Which artifact class this rule governs.
    pub artifact: LearningArtifactKind,
    /// Canonical live storage plane.
    pub primary_plane: StoragePlane,
    /// How long the full-fidelity live form should remain the default access
    /// path.
    pub hot_days: u16,
    /// When compaction should begin, if any.
    pub compact_after_days: Option<u16>,
    /// When the live/raw form may be intentionally dropped, if ever.
    pub drop_after_days: Option<u16>,
    /// How the artifact is reduced over time.
    pub compaction_strategy: CompactionStrategy,
    /// What event or cadence permits Git promotion.
    pub archive_trigger: ArchiveTrigger,
    /// Archive discoverability contract.
    pub archive_retention: ArchiveRetention,
    /// How future runs remain comparable after compaction/archive.
    pub comparability_anchor: ComparabilityAnchor,
    /// Whether the artifact must remain directly queryable while unresolved.
    pub must_remain_queryable_while_open: bool,
    /// Operator-facing explanation of how old/live/archive behavior should read.
    pub operator_story: &'static str,
}

impl ArtifactRetentionRule {
    /// Whether this artifact class ever has a Git archive form.
    #[must_use]
    pub const fn has_git_archive_path(&self) -> bool {
        !matches!(self.archive_trigger, ArchiveTrigger::Never)
    }

    /// Whether this artifact class is archived under the normal policy flow
    /// without requiring a manual forensic promotion step.
    #[must_use]
    pub const fn archives_to_git_by_default(&self) -> bool {
        matches!(
            self.archive_trigger,
            ArchiveTrigger::OnPolicyPromotionOrRollback
                | ArchiveTrigger::OnRegimeChange
                | ArchiveTrigger::PeriodicAudit
        )
    }

    /// Whether this artifact class only reaches Git after an explicit
    /// selection/promote action.
    #[must_use]
    pub const fn requires_explicit_promotion(&self) -> bool {
        matches!(
            self.archive_trigger,
            ArchiveTrigger::SelectedForensicPromotion
        )
    }
}

/// Canonical retention rules for the ATC learning stack.
pub const ATC_RETENTION_RULES: &[ArtifactRetentionRule] = &[
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::OpenExperienceRows,
        primary_plane: StoragePlane::LiveSqlite,
        hot_days: OPEN_EXPERIENCE_TERMINALIZE_AFTER_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::KeepRawUntilTerminal,
        archive_trigger: ArchiveTrigger::Never,
        archive_retention: ArchiveRetention::Never,
        comparability_anchor: ComparabilityAnchor::PolicyEpochStratumWindow,
        must_remain_queryable_while_open: true,
        operator_story: "Never hide open experiences behind compaction. After 7 days \
                         they are stale and must surface as operator work; by 30 days \
                         they must be resolved, censored, expired, or explicitly held \
                         for a forensic reason.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::ResolvedExperienceRows,
        primary_plane: StoragePlane::LiveSqlite,
        hot_days: RESOLVED_EXPERIENCE_FULL_FIDELITY_DAYS,
        compact_after_days: Some(RESOLVED_EXPERIENCE_FULL_FIDELITY_DAYS),
        drop_after_days: Some(RESOLVED_EXPERIENCE_DROP_AFTER_DAYS),
        compaction_strategy: CompactionStrategy::RollUpAndSelectExemplars,
        archive_trigger: ArchiveTrigger::Never,
        archive_retention: ArchiveRetention::Never,
        comparability_anchor: ComparabilityAnchor::PolicyEpochStratumWindow,
        must_remain_queryable_while_open: false,
        operator_story: "Full row-level detail is first-class for 30 days. After that, \
                         operator surfaces should prefer rollups plus exemplar pointers; \
                         raw rows may be dropped after 365 days once rollups and promoted \
                         artifacts preserve the learning/audit story.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::ExperienceRollups,
        primary_plane: StoragePlane::CompactedSqlite,
        hot_days: ROLLUP_LIVE_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::IncrementalRollupOnly,
        archive_trigger: ArchiveTrigger::Never,
        archive_retention: ArchiveRetention::Never,
        comparability_anchor: ComparabilityAnchor::PolicyEpochStratumWindow,
        must_remain_queryable_while_open: false,
        operator_story: "Rollups are the bounded warm path and should outlive raw rows. \
                         They remain queryable in SQLite even after raw-row retention ends; \
                         Git receives audit summaries derived from them, not the rollup table itself.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::EvidenceLedgerEntries,
        primary_plane: StoragePlane::EvidenceLedger,
        hot_days: EVIDENCE_LEDGER_HOT_DAYS,
        compact_after_days: Some(EVIDENCE_LEDGER_HOT_DAYS),
        drop_after_days: Some(EVIDENCE_LEDGER_DROP_AFTER_DAYS),
        compaction_strategy: CompactionStrategy::BoundedDebugTail,
        archive_trigger: ArchiveTrigger::Never,
        archive_retention: ArchiveRetention::Never,
        comparability_anchor: ComparabilityAnchor::None,
        must_remain_queryable_while_open: false,
        operator_story: "The evidence ledger is for recent explainability and debugging, \
                         not long-term canonical history. Keep a short hot tail, then drop \
                         it once policy/audit artifacts summarize any material decisions.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::PolicyBundleSnapshots,
        primary_plane: StoragePlane::GitArchive,
        hot_days: POLICY_SNAPSHOT_HOT_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::ImmutableSnapshot,
        archive_trigger: ArchiveTrigger::OnPolicyPromotionOrRollback,
        archive_retention: ArchiveRetention::Indefinite,
        comparability_anchor: ComparabilityAnchor::PolicySnapshotComparator,
        must_remain_queryable_while_open: false,
        operator_story: "Every promoted or rolled-back policy bundle gets an immutable Git \
                         snapshot with links to the prior comparable bundle and baseline. \
                         Live surfaces may treat older bundles as historical, but never as absent.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::RegimeSummaries,
        primary_plane: StoragePlane::GitArchive,
        hot_days: STALE_REGIME_AFTER_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::ImmutableSummary,
        archive_trigger: ArchiveTrigger::OnRegimeChange,
        archive_retention: ArchiveRetention::MinimumDays(ARCHIVE_DISCOVERABILITY_MIN_DAYS),
        comparability_anchor: ComparabilityAnchor::PolicySnapshotComparator,
        must_remain_queryable_while_open: false,
        operator_story: "Regimes older than 14 days without supporting evidence are stale. \
                         Keep their summary discoverable in Git with before/after pointers so \
                         operators can tell what changed without mistaking stale state for live state.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::TransparencyCards,
        primary_plane: StoragePlane::EvidenceLedger,
        hot_days: POLICY_SNAPSHOT_HOT_DAYS,
        compact_after_days: Some(POLICY_SNAPSHOT_HOT_DAYS),
        drop_after_days: Some(RESOLVED_EXPERIENCE_DROP_AFTER_DAYS),
        compaction_strategy: CompactionStrategy::BatchIntoAuditSummaries,
        archive_trigger: ArchiveTrigger::Never,
        archive_retention: ArchiveRetention::Never,
        comparability_anchor: ComparabilityAnchor::PolicySnapshotComparator,
        must_remain_queryable_while_open: false,
        operator_story: "Transparency cards stay queryable on the warm path long enough for \
                         operators to inspect recent mathematical decisions, then they should \
                         be folded into periodic/event-driven audit summaries rather than mirrored \
                         one-by-one into Git.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::AuditSummaries,
        primary_plane: StoragePlane::GitArchive,
        hot_days: PERIODIC_AUDIT_CADENCE_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::ImmutableSummary,
        archive_trigger: ArchiveTrigger::PeriodicAudit,
        archive_retention: ArchiveRetention::MinimumDays(ARCHIVE_DISCOVERABILITY_MIN_DAYS),
        comparability_anchor: ComparabilityAnchor::PolicySnapshotComparator,
        must_remain_queryable_while_open: false,
        operator_story: "Git should contain concise periodic or event-driven audit summaries \
                         that point to deeper evidence. Emit them on a bounded cadence or \
                         material event, not per tick or per row.",
    },
    ArtifactRetentionRule {
        artifact: LearningArtifactKind::ExemplarTraces,
        primary_plane: StoragePlane::GitArchive,
        hot_days: POLICY_SNAPSHOT_HOT_DAYS,
        compact_after_days: None,
        drop_after_days: None,
        compaction_strategy: CompactionStrategy::PromoteSelectedForensics,
        archive_trigger: ArchiveTrigger::SelectedForensicPromotion,
        archive_retention: ArchiveRetention::MinimumDays(FORENSIC_TRACE_DISCOVERABILITY_DAYS),
        comparability_anchor: ComparabilityAnchor::ScenarioReplayComparator,
        must_remain_queryable_while_open: false,
        operator_story: "Promote only selected scenario-linked exemplar traces and replay \
                         manifests into Git. They exist to preserve verification comparability, \
                         not to mirror every raw decision.",
    },
];

/// Artifact classes that are emitted into Git as part of the normal archive
/// policy flow.
pub const GIT_ARCHIVE_DEFAULT_ARTIFACTS: &[LearningArtifactKind] = &[
    LearningArtifactKind::PolicyBundleSnapshots,
    LearningArtifactKind::RegimeSummaries,
    LearningArtifactKind::AuditSummaries,
];

/// Artifact classes that may enter Git only after an explicit forensic
/// promotion step.
pub const GIT_ARCHIVE_PROMOTION_ONLY_ARTIFACTS: &[LearningArtifactKind] =
    &[LearningArtifactKind::ExemplarTraces];

/// Artifact classes that must stay out of Git by default.
pub const GIT_ARCHIVE_DENYLIST: &[LearningArtifactKind] = &[
    LearningArtifactKind::OpenExperienceRows,
    LearningArtifactKind::ResolvedExperienceRows,
    LearningArtifactKind::ExperienceRollups,
    LearningArtifactKind::EvidenceLedgerEntries,
    LearningArtifactKind::TransparencyCards,
];

/// High-volume exhaust that must not be archived as standalone Git artifacts.
pub const GIT_ARCHIVE_EXPLICIT_EXCLUSIONS: &[&str] = &[
    "raw atc_experiences row mirrors",
    "raw atc_experience_rollups table dumps",
    "per-tick posterior vectors or controller internals",
    "evidence-ledger JSONL tails",
    "duplicate resolver retries or suppression bookkeeping noise",
];

/// Operator-facing lifecycle rules that all future surfaces should respect.
pub const OPERATOR_LIFECYCLE_RULES: &[&str] = &[
    "Open experiences older than 7 days are stale work, not background noise.",
    "Open experiences older than 30 days require resolution, censoring, expiry, or an explicit forensic hold.",
    "When raw rows are compacted away, surfaces must say the view is rollup-backed or archive-backed rather than silently truncating history.",
    "Regimes older than 14 days without fresh evidence are historical and must be labeled stale/inactive.",
    "Archived artifacts must link back to the live policy/regime/evidence identifiers they summarize.",
];

/// Replay-comparability requirements that cold-path artifacts must satisfy.
pub const REPLAY_DISCOVERABILITY_REQUIREMENTS: &[&str] = &[
    "carry the active policy or bundle identifier",
    "carry the comparator/baseline identifier used for before-vs-after judgement",
    "record the covered time window or scenario identifier",
    "point to the immediately previous comparable artifact when one exists",
    "retain enough IDs (decision, experience, stratum, or replay hash) to bridge back to durable evidence",
];

/// Return the canonical retention rule for an artifact kind.
#[must_use]
pub fn retention_rule(kind: LearningArtifactKind) -> Option<&'static ArtifactRetentionRule> {
    ATC_RETENTION_RULES
        .iter()
        .find(|rule| rule.artifact == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_artifact_kind_has_one_rule() {
        let all = [
            LearningArtifactKind::OpenExperienceRows,
            LearningArtifactKind::ResolvedExperienceRows,
            LearningArtifactKind::ExperienceRollups,
            LearningArtifactKind::EvidenceLedgerEntries,
            LearningArtifactKind::PolicyBundleSnapshots,
            LearningArtifactKind::RegimeSummaries,
            LearningArtifactKind::TransparencyCards,
            LearningArtifactKind::AuditSummaries,
            LearningArtifactKind::ExemplarTraces,
        ];
        for kind in all {
            assert!(retention_rule(kind).is_some(), "missing rule for {kind:?}");
        }
        assert_eq!(ATC_RETENTION_RULES.len(), all.len());
    }

    #[test]
    fn git_archive_lists_do_not_overlap() {
        for allowed in GIT_ARCHIVE_DEFAULT_ARTIFACTS {
            assert!(
                !GIT_ARCHIVE_DENYLIST.contains(allowed),
                "{allowed:?} appears in both archive lists"
            );
        }
        for promoted in GIT_ARCHIVE_PROMOTION_ONLY_ARTIFACTS {
            assert!(
                !GIT_ARCHIVE_DENYLIST.contains(promoted),
                "{promoted:?} appears in both archive lists"
            );
            assert!(
                !GIT_ARCHIVE_DEFAULT_ARTIFACTS.contains(promoted),
                "{promoted:?} appears in both default and promotion-only archive lists"
            );
        }
    }

    #[test]
    fn denylisted_artifacts_never_archive_by_default() {
        for kind in GIT_ARCHIVE_DENYLIST {
            let rule = retention_rule(*kind).expect("rule");
            assert!(
                !rule.archives_to_git_by_default(),
                "{kind:?} must stay out of Git by default"
            );
            assert!(
                matches!(rule.archive_retention, ArchiveRetention::Never),
                "{kind:?} should not declare archive retention"
            );
        }
    }

    #[test]
    fn default_archived_artifacts_have_archive_contracts() {
        for kind in GIT_ARCHIVE_DEFAULT_ARTIFACTS {
            let rule = retention_rule(*kind).expect("rule");
            assert!(
                rule.archives_to_git_by_default(),
                "{kind:?} should archive to Git"
            );
            assert!(
                !rule.requires_explicit_promotion(),
                "{kind:?} should not require explicit promotion"
            );
            assert!(
                rule.archive_retention.enabled(),
                "{kind:?} must declare archive discoverability"
            );
            assert!(
                !matches!(rule.comparability_anchor, ComparabilityAnchor::None),
                "{kind:?} must remain comparable after archive"
            );
        }
    }

    #[test]
    fn promotion_only_artifacts_require_explicit_selection() {
        for kind in GIT_ARCHIVE_PROMOTION_ONLY_ARTIFACTS {
            let rule = retention_rule(*kind).expect("rule");
            assert!(
                rule.has_git_archive_path(),
                "{kind:?} should still have an archive path"
            );
            assert!(
                !rule.archives_to_git_by_default(),
                "{kind:?} should not archive by default"
            );
            assert!(
                rule.requires_explicit_promotion(),
                "{kind:?} should require explicit promotion"
            );
            assert!(
                rule.archive_retention.enabled(),
                "{kind:?} must still declare archive discoverability"
            );
        }
    }

    #[test]
    fn open_rows_remain_queryable_until_terminal() {
        let rule = retention_rule(LearningArtifactKind::OpenExperienceRows).expect("rule");
        assert!(rule.must_remain_queryable_while_open);
        assert!(rule.drop_after_days.is_none());
        assert_eq!(rule.hot_days, OPEN_EXPERIENCE_TERMINALIZE_AFTER_DAYS);
    }

    #[test]
    fn resolved_raw_rows_age_out_but_rollups_do_not() {
        let raw = retention_rule(LearningArtifactKind::ResolvedExperienceRows).expect("rule");
        let rollups = retention_rule(LearningArtifactKind::ExperienceRollups).expect("rule");
        assert_eq!(
            raw.drop_after_days,
            Some(RESOLVED_EXPERIENCE_DROP_AFTER_DAYS)
        );
        assert!(rollups.drop_after_days.is_none());
        assert!(
            rollups.hot_days > raw.hot_days,
            "rollups should outlive row-level hot retention"
        );
    }

    #[test]
    fn evidence_ledger_is_short_lived_relative_to_learning_store() {
        let ledger = retention_rule(LearningArtifactKind::EvidenceLedgerEntries).expect("rule");
        let raw = retention_rule(LearningArtifactKind::ResolvedExperienceRows).expect("rule");
        assert!(ledger.hot_days < raw.hot_days);
        assert!(
            ledger.drop_after_days.expect("ledger drop") < raw.drop_after_days.expect("raw drop")
        );
    }

    #[test]
    fn archived_artifacts_outlive_raw_rows_for_forensics() {
        let raw = retention_rule(LearningArtifactKind::ResolvedExperienceRows).expect("rule");
        let raw_drop = raw.drop_after_days.expect("raw drop");
        for kind in [
            LearningArtifactKind::RegimeSummaries,
            LearningArtifactKind::AuditSummaries,
            LearningArtifactKind::ExemplarTraces,
        ] {
            let rule = retention_rule(kind).expect("rule");
            let archive_days = rule.archive_retention.minimum_days().expect("archive days");
            assert!(
                archive_days >= raw_drop,
                "{kind:?} should remain discoverable at least as long as raw rows"
            );
        }
    }

    #[test]
    fn periodic_audit_is_bounded_and_low_write_amplification() {
        assert!(PERIODIC_AUDIT_CADENCE_DAYS >= 1);
        assert!(
            PERIODIC_AUDIT_CADENCE_DAYS <= OPEN_EXPERIENCE_STALE_AFTER_DAYS,
            "audit cadence should not exceed the stale-open-experience threshold"
        );
        assert!(!GIT_ARCHIVE_EXPLICIT_EXCLUSIONS.is_empty());
    }
}
