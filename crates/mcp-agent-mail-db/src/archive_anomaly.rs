//! Archive anomaly taxonomy and safe remediation classes (br-97gc6.5.2.4.1).
//!
//! This module is the single source of truth for classifying archive-level
//! anomalies and determining which remediation actions are safe for each class.
//! It is consumed by the CLI (`doctor archive-scan`, `doctor archive-normalize`),
//! the server (system health TUI), and the mailbox supervisor (automated
//! normalization workflows).
//!
//! # Design Principles
//!
//! 1. **Non-destructive by default.** No remediation action deletes data.
//!    The most aggressive automatic action is quarantine (rename aside).
//! 2. **Typed over stringly-typed.** Every anomaly class has a variant in
//!    [`ArchiveAnomalyKind`] so downstream code can pattern-match rather than
//!    substring-match on human-readable messages.
//! 3. **Severity is intrinsic** to the anomaly kind (not assigned ad-hoc by
//!    the scanner). This ensures consistent triage across all surfaces.
//! 4. **Remediation is classified, not executed** here. This module says
//!    *what class* of fix is appropriate; the scanner and normalizer modules
//!    decide *when* to apply it.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

// ============================================================================
// Anomaly severity
// ============================================================================

/// How severe an archive anomaly is, from least to most urgent.
///
/// Severity determines default sort order, whether the finding appears in
/// concise summaries, and whether it blocks promotion from `Stale` → `Healthy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalySeverity {
    /// Informational oddity that does not affect correctness.
    Info,
    /// Something is suboptimal but data integrity is not at risk.
    Warning,
    /// Data integrity may be compromised; remediation recommended.
    Error,
    /// Archive is unsafe for reconstruction; operator attention required.
    Critical,
}

impl AnomalySeverity {
    /// Numeric level for sorting (higher = worse).
    #[must_use]
    pub const fn level(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warning => 1,
            Self::Error => 2,
            Self::Critical => 3,
        }
    }

    /// Human-readable label for CLI output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }
}

impl std::fmt::Display for AnomalySeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// Remediation classes
// ============================================================================

/// What class of remediation is safe for a given anomaly.
///
/// These classes form a trust hierarchy: `ReportOnly` needs zero authority,
/// `SafeAuto` can be run unattended, `NeedsConfirmation` requires an operator
/// prompt, and `ManualOnly` must be handled entirely by a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationClass {
    /// No safe automatic action exists; report the finding and move on.
    /// The operator can investigate at their discretion.
    ReportOnly,

    /// A non-destructive fix can be applied without confirmation.
    /// Examples: writing a missing `project.json` from known-good data,
    /// annotating a canonical file with a correction marker.
    SafeAuto,

    /// A non-destructive fix exists but its side effects warrant an
    /// explicit operator confirmation before execution.
    /// Examples: quarantining duplicate canonical files (renames them aside),
    /// rewriting project metadata when the canonical slug is ambiguous.
    NeedsConfirmation,

    /// No automated remediation is safe. The operator must manually
    /// inspect and resolve the anomaly.
    /// Examples: orphaned agents with no parent project, archive files
    /// with corrupted binary content, identity conflicts that cannot be
    /// disambiguated programmatically.
    ManualOnly,
}

impl RemediationClass {
    /// Human-readable label for CLI/JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReportOnly => "report_only",
            Self::SafeAuto => "safe_auto",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::ManualOnly => "manual_only",
        }
    }

    /// Whether this class permits any automatic action (with or without
    /// confirmation).
    #[must_use]
    pub const fn has_automated_action(self) -> bool {
        matches!(self, Self::SafeAuto | Self::NeedsConfirmation)
    }

    /// Whether this class can proceed without operator interaction.
    #[must_use]
    pub const fn is_unattended(self) -> bool {
        matches!(self, Self::SafeAuto)
    }
}

impl std::fmt::Display for RemediationClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// Anomaly kinds
// ============================================================================

/// Exhaustive classification of archive anomaly types.
///
/// Each variant carries the minimum structured data needed to render a useful
/// finding. The [`ArchiveAnomalyKind::severity`] and
/// [`ArchiveAnomalyKind::remediation_class`] methods return the intrinsic
/// classification for the anomaly type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArchiveAnomalyKind {
    // -- Duplicate canonical IDs -----------------------------------------
    /// Two or more archive `.md` files resolve to the same positive message id.
    DuplicateCanonicalId {
        /// The duplicated message id.
        message_id: i64,
        /// Path to the file that will be kept (first encountered).
        keep_path: PathBuf,
        /// Paths to the duplicate files.
        duplicate_paths: Vec<PathBuf>,
    },

    // -- Malformed message frontmatter -----------------------------------
    /// A `.md` file under `messages/YYYY/MM/` has no JSON frontmatter block.
    MissingFrontmatter {
        /// Path to the affected file.
        path: PathBuf,
    },

    /// A `.md` file has a JSON frontmatter block that fails to parse.
    UnparseableFrontmatter {
        /// Path to the affected file.
        path: PathBuf,
        /// The parse error message.
        parse_error: String,
    },

    /// Frontmatter parses as valid JSON but is missing the required `id` field
    /// or the id is not a positive integer.
    InvalidMessageId {
        /// Path to the affected file.
        path: PathBuf,
        /// Description of what is wrong with the id (missing, zero, negative).
        detail: String,
    },

    /// Frontmatter is valid JSON with a positive id, but required fields
    /// (`from`, `to`, `subject`, and a created timestamp) are missing or malformed.
    IncompleteFrontmatter {
        /// Path to the affected file.
        path: PathBuf,
        /// List of missing or invalid required field names.
        missing_fields: Vec<String>,
    },

    // -- Project metadata ------------------------------------------------
    /// A project directory has no `project.json` file.
    MissingProjectMetadata {
        /// Path to the project directory.
        project_dir: PathBuf,
        /// Fallback slug derived from the directory name.
        fallback_slug: String,
    },

    /// `project.json` exists but contains invalid JSON or is missing
    /// required fields (`slug`, `human_key`).
    InvalidProjectMetadata {
        /// Path to the `project.json` file.
        path: PathBuf,
        /// The slug (possibly from fallback).
        slug: String,
        /// A canonical human_key if one can be inferred from the DB or
        /// directory structure.
        canonical_human_key: Option<String>,
        /// Description of the problem.
        detail: String,
    },

    // -- Suspicious / ephemeral projects ---------------------------------
    /// A project appears to be ephemeral (tmp, dev, test prefix/root) and
    /// should not be in the production archive.
    SuspiciousEphemeralProject {
        /// Path to the project directory.
        project_dir: PathBuf,
        /// The project slug.
        slug: String,
        /// The human_key, if available.
        human_key: Option<String>,
        /// Why this project is considered suspicious.
        reason: String,
    },

    // -- Orphaned / inconsistent agents ----------------------------------
    /// An agent profile directory exists under a project that is not itself
    /// represented in the archive or DB.
    OrphanedAgentProfile {
        /// Path to the agent's `profile.json`.
        profile_path: PathBuf,
        /// The agent name from the directory.
        agent_name: String,
        /// The parent project directory that is missing or unrecognized.
        parent_project_dir: PathBuf,
    },

    /// An agent profile's `profile.json` is missing or unparseable.
    MalformedAgentProfile {
        /// Path to the expected `profile.json`.
        profile_path: PathBuf,
        /// The agent name from the directory.
        agent_name: String,
        /// Description of the problem (missing file, parse error, etc.).
        detail: String,
    },

    // -- Archive structure anomalies -------------------------------------
    /// A year or month directory under `messages/` has an unexpected name
    /// (not 4-digit year or 2-digit month).
    InvalidDateDirectory {
        /// Path to the malformed directory.
        path: PathBuf,
        /// Whether this is a year-level or month-level directory.
        level: DateDirectoryLevel,
        /// The actual directory name.
        name: String,
    },

    /// A file exists under `messages/YYYY/MM/` that is not a `.md` file.
    UnexpectedFileInMessageDir {
        /// Path to the unexpected file.
        path: PathBuf,
    },

    /// A symlink was found where a real directory or file was expected.
    /// Symlinks in the archive are never canonical and may indicate
    /// filesystem-level tampering or misconfigured storage.
    UnexpectedSymlink {
        /// Path to the symlink.
        path: PathBuf,
        /// What the symlink points to (if resolvable).
        target: Option<PathBuf>,
    },

    // -- Identity mismatches ---------------------------------------------
    /// The archive contains a project identity that does not match any
    /// project in the database. This may indicate archive drift or a
    /// project that was deleted from the DB but not the archive.
    ArchiveDbProjectMismatch {
        /// The archive-side identity.
        archive_slug: String,
        /// The archive-side human_key, if available.
        archive_human_key: Option<String>,
        /// Description of the mismatch.
        detail: String,
    },

    /// The message count in the archive differs significantly from the DB.
    ArchiveDbCountDrift {
        /// Number of unique message ids in the archive.
        archive_count: usize,
        /// Number of messages in the DB.
        db_count: usize,
        /// The absolute difference.
        drift: usize,
    },

    // -- Archive/DB artifact cross-checks -------------------------------
    /// A message row exists in SQLite but no canonical archive message file
    /// carries the same message id.
    MissingCanonicalMessage {
        /// Project slug from the DB row.
        project_slug: String,
        /// Message id from the DB row.
        message_id: i64,
        /// Subject from the DB row, for operator triage.
        db_subject: String,
        /// Sender name from the DB row.
        db_sender: String,
    },

    /// A DB sender/recipient row is missing its expected inbox/outbox archive copy.
    MessageRecipientCopyMismatch {
        /// Project slug from the DB row.
        project_slug: String,
        /// Message id whose mailbox copy is missing or inconsistent.
        message_id: i64,
        /// Agent whose mailbox copy was checked.
        agent_name: String,
        /// Which mailbox copy was checked.
        mailbox: MailboxCopyKind,
        /// One-line mismatch detail.
        detail: String,
    },

    /// A DB agent row is missing a profile artifact or the artifact disagrees
    /// with the DB identity fields.
    AgentProfileMismatch {
        /// Project slug from the DB row.
        project_slug: String,
        /// Agent name from the DB row.
        agent_name: String,
        /// Expected archive profile path.
        profile_path: PathBuf,
        /// One-line mismatch detail.
        detail: String,
    },

    /// A DB file reservation row is missing its stable id artifact or the
    /// artifact disagrees with the DB reservation fields.
    FileReservationArtifactMismatch {
        /// Project slug from the DB row.
        project_slug: String,
        /// File reservation id from the DB row.
        reservation_id: i64,
        /// Expected archive artifact path.
        artifact_path: PathBuf,
        /// One-line mismatch detail.
        detail: String,
    },
}

/// Whether an [`InvalidDateDirectory`](ArchiveAnomalyKind::InvalidDateDirectory)
/// is at the year or month level of the `messages/YYYY/MM/` hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateDirectoryLevel {
    Year,
    Month,
}

impl std::fmt::Display for DateDirectoryLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Year => f.write_str("year"),
            Self::Month => f.write_str("month"),
        }
    }
}

/// Which per-agent mailbox copy was checked by the archive/DB verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxCopyKind {
    Inbox,
    Outbox,
}

impl MailboxCopyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Outbox => "outbox",
        }
    }
}

impl std::fmt::Display for MailboxCopyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ArchiveAnomalyKind {
    /// Intrinsic severity for this anomaly type.
    #[must_use]
    pub const fn severity(&self) -> AnomalySeverity {
        match self {
            // Duplicates are an error — reconstruction will skip them but data
            // may have diverged between the copies.
            Self::DuplicateCanonicalId { .. } => AnomalySeverity::Error,

            // Malformed frontmatter is an error — the message cannot be
            // recovered without manual inspection.
            Self::MissingFrontmatter { .. }
            | Self::UnparseableFrontmatter { .. }
            | Self::InvalidMessageId { .. } => AnomalySeverity::Error,

            // Incomplete frontmatter is a warning — the message id is valid
            // so reconstruction can partially recover it.
            Self::IncompleteFrontmatter { .. } => AnomalySeverity::Warning,

            // Missing project metadata is a warning — the directory name
            // provides a usable fallback slug.
            Self::MissingProjectMetadata { .. } => AnomalySeverity::Warning,

            // Invalid project metadata is a warning or error depending on
            // whether a canonical value can be inferred.
            Self::InvalidProjectMetadata {
                canonical_human_key,
                ..
            } => {
                if canonical_human_key.is_some() {
                    AnomalySeverity::Warning
                } else {
                    AnomalySeverity::Error
                }
            }

            // Suspicious ephemeral projects are informational — they are
            // valid archives, just not production-grade.
            Self::SuspiciousEphemeralProject { .. } => AnomalySeverity::Info,

            // Orphaned agents are an error — they cannot be associated with
            // a project during reconstruction.
            Self::OrphanedAgentProfile { .. } => AnomalySeverity::Error,

            // Malformed agent profiles are a warning — the agent directory
            // name still identifies the agent.
            Self::MalformedAgentProfile { .. } => AnomalySeverity::Warning,

            // Structural anomalies are informational — they don't affect
            // reconstruction of properly-formed data.
            Self::InvalidDateDirectory { .. }
            | Self::UnexpectedFileInMessageDir { .. }
            | Self::UnexpectedSymlink { .. } => AnomalySeverity::Info,

            // Identity mismatches are warnings — they indicate drift but
            // both archive and DB remain internally consistent.
            Self::ArchiveDbProjectMismatch { .. } => AnomalySeverity::Warning,

            // Count drift is severity-dependent on magnitude, but the kind
            // itself is classified as warning. Callers can upgrade based on
            // the `drift` magnitude.
            Self::ArchiveDbCountDrift { .. } => AnomalySeverity::Warning,

            Self::MissingCanonicalMessage { .. } => AnomalySeverity::Error,
            Self::MessageRecipientCopyMismatch { .. }
            | Self::AgentProfileMismatch { .. }
            | Self::FileReservationArtifactMismatch { .. } => AnomalySeverity::Warning,
        }
    }

    /// The safe remediation class for this anomaly type.
    #[must_use]
    pub const fn remediation_class(&self) -> RemediationClass {
        match self {
            // Duplicate canonical files: quarantine (rename aside) the extras.
            // Needs confirmation because it changes the archive directory layout.
            Self::DuplicateCanonicalId { .. } => RemediationClass::NeedsConfirmation,

            // Missing/unparseable frontmatter: we cannot synthesize valid
            // message content, so report only.
            Self::MissingFrontmatter { .. } | Self::UnparseableFrontmatter { .. } => {
                RemediationClass::ReportOnly
            }

            // Invalid message id: the frontmatter may be repairable if we
            // can derive the id from the filename, but that is too risky
            // without human review.
            Self::InvalidMessageId { .. } => RemediationClass::ManualOnly,

            // Incomplete frontmatter: the message can be partially recovered
            // but missing fields need human decision.
            Self::IncompleteFrontmatter { .. } => RemediationClass::ReportOnly,

            // Missing project metadata: we can safely write a `project.json`
            // from the directory name (fallback slug).
            Self::MissingProjectMetadata { .. } => RemediationClass::SafeAuto,

            // Invalid project metadata with known canonical value: safe auto
            // rewrite. Without canonical value: needs manual resolution.
            Self::InvalidProjectMetadata {
                canonical_human_key,
                ..
            } => {
                if canonical_human_key.is_some() {
                    RemediationClass::SafeAuto
                } else {
                    RemediationClass::ManualOnly
                }
            }

            // Suspicious ephemeral projects: report only. The operator
            // decides whether to remove or reclassify them.
            Self::SuspiciousEphemeralProject { .. } => RemediationClass::ReportOnly,

            // Orphaned agents: manual only. We cannot safely create or
            // associate a parent project without operator guidance.
            Self::OrphanedAgentProfile { .. } => RemediationClass::ManualOnly,

            // Malformed agent profiles: report only. The agent directory
            // still identifies the agent for reconstruction.
            Self::MalformedAgentProfile { .. } => RemediationClass::ReportOnly,

            // Structural oddities: report only.
            Self::InvalidDateDirectory { .. }
            | Self::UnexpectedFileInMessageDir { .. }
            | Self::UnexpectedSymlink { .. } => RemediationClass::ReportOnly,

            // Identity mismatches: report only. Resolving drift requires
            // understanding which side is authoritative.
            Self::ArchiveDbProjectMismatch { .. } => RemediationClass::ReportOnly,

            // Count drift: report only. The actual resolution depends on
            // which messages are missing from which side.
            Self::ArchiveDbCountDrift { .. } => RemediationClass::ReportOnly,

            // Missing canonical content cannot be synthesized from the DB
            // without deciding whether the archive or DB is authoritative.
            Self::MissingCanonicalMessage { .. } => RemediationClass::ManualOnly,

            // Copy/profile/reservation drift is evidence, not an executable
            // repair plan. Report it and let higher-level doctor workflows
            // point operators at reconstruction, normalization, or inspection.
            Self::MessageRecipientCopyMismatch { .. }
            | Self::AgentProfileMismatch { .. }
            | Self::FileReservationArtifactMismatch { .. } => RemediationClass::ReportOnly,
        }
    }

    /// Short machine-readable tag for this anomaly kind (without payload).
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::DuplicateCanonicalId { .. } => "duplicate_canonical_id",
            Self::MissingFrontmatter { .. } => "missing_frontmatter",
            Self::UnparseableFrontmatter { .. } => "unparseable_frontmatter",
            Self::InvalidMessageId { .. } => "invalid_message_id",
            Self::IncompleteFrontmatter { .. } => "incomplete_frontmatter",
            Self::MissingProjectMetadata { .. } => "missing_project_metadata",
            Self::InvalidProjectMetadata { .. } => "invalid_project_metadata",
            Self::SuspiciousEphemeralProject { .. } => "suspicious_ephemeral_project",
            Self::OrphanedAgentProfile { .. } => "orphaned_agent_profile",
            Self::MalformedAgentProfile { .. } => "malformed_agent_profile",
            Self::InvalidDateDirectory { .. } => "invalid_date_directory",
            Self::UnexpectedFileInMessageDir { .. } => "unexpected_file_in_message_dir",
            Self::UnexpectedSymlink { .. } => "unexpected_symlink",
            Self::ArchiveDbProjectMismatch { .. } => "archive_db_project_mismatch",
            Self::ArchiveDbCountDrift { .. } => "archive_db_count_drift",
            Self::MissingCanonicalMessage { .. } => "missing_canonical_message",
            Self::MessageRecipientCopyMismatch { .. } => "message_recipient_copy_mismatch",
            Self::AgentProfileMismatch { .. } => "agent_profile_mismatch",
            Self::FileReservationArtifactMismatch { .. } => "file_reservation_artifact_mismatch",
        }
    }

    /// One-line human-readable summary of this anomaly.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::DuplicateCanonicalId {
                message_id,
                duplicate_paths,
                ..
            } => format!(
                "message id {message_id} has {} duplicate archive file(s)",
                duplicate_paths.len()
            ),
            Self::MissingFrontmatter { path } => {
                format!("no JSON frontmatter in {}", path.display())
            }
            Self::UnparseableFrontmatter { path, parse_error } => {
                format!("bad JSON frontmatter in {}: {parse_error}", path.display())
            }
            Self::InvalidMessageId { path, detail } => {
                format!("invalid message id in {}: {detail}", path.display())
            }
            Self::IncompleteFrontmatter {
                path,
                missing_fields,
            } => format!(
                "incomplete frontmatter in {} (missing: {})",
                path.display(),
                missing_fields.join(", ")
            ),
            Self::MissingProjectMetadata {
                project_dir,
                fallback_slug,
            } => format!(
                "missing project.json in {} (fallback slug: {fallback_slug})",
                project_dir.display()
            ),
            Self::InvalidProjectMetadata {
                path, slug, detail, ..
            } => format!(
                "invalid project.json at {} for {slug}: {detail}",
                path.display()
            ),
            Self::SuspiciousEphemeralProject { slug, reason, .. } => {
                format!("suspicious project '{slug}': {reason}")
            }
            Self::OrphanedAgentProfile {
                agent_name,
                parent_project_dir,
                ..
            } => format!(
                "orphaned agent '{agent_name}' under unrecognized project {}",
                parent_project_dir.display()
            ),
            Self::MalformedAgentProfile {
                agent_name, detail, ..
            } => format!("malformed profile for agent '{agent_name}': {detail}"),
            Self::InvalidDateDirectory {
                path, level, name, ..
            } => format!(
                "invalid {level} directory name '{name}' at {}",
                path.display()
            ),
            Self::UnexpectedFileInMessageDir { path } => {
                format!("unexpected non-.md file at {}", path.display())
            }
            Self::UnexpectedSymlink { path, target } => {
                let suffix = target
                    .as_ref()
                    .map_or(String::new(), |t| format!(" → {}", t.display()));
                format!("unexpected symlink at {}{suffix}", path.display())
            }
            Self::ArchiveDbProjectMismatch {
                archive_slug,
                detail,
                ..
            } => format!("archive/DB project mismatch for '{archive_slug}': {detail}"),
            Self::ArchiveDbCountDrift {
                archive_count,
                db_count,
                drift,
            } => format!(
                "message count drift: archive={archive_count}, db={db_count} (delta={drift})"
            ),
            Self::MissingCanonicalMessage {
                project_slug,
                message_id,
                db_subject,
                db_sender,
            } => format!(
                "DB message {project_slug}/{message_id} from '{db_sender}' has no canonical archive file (subject: {db_subject})"
            ),
            Self::MessageRecipientCopyMismatch {
                project_slug,
                message_id,
                agent_name,
                mailbox,
                detail,
            } => format!(
                "{mailbox} copy mismatch for {project_slug}/{message_id}/{agent_name}: {detail}"
            ),
            Self::AgentProfileMismatch {
                project_slug,
                agent_name,
                detail,
                ..
            } => format!("agent profile mismatch for {project_slug}/{agent_name}: {detail}"),
            Self::FileReservationArtifactMismatch {
                project_slug,
                reservation_id,
                detail,
                ..
            } => {
                format!(
                    "file reservation artifact mismatch for {project_slug}/{reservation_id}: {detail}"
                )
            }
        }
    }
}

impl std::fmt::Display for ArchiveAnomalyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.severity(), self.summary())
    }
}

// ============================================================================
// Anomaly finding (kind + context)
// ============================================================================

/// A single archive anomaly finding with full context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveAnomaly {
    /// The classified anomaly.
    pub kind: ArchiveAnomalyKind,
    /// Timestamp when the anomaly was detected (microseconds since epoch).
    pub detected_at: i64,
}

impl ArchiveAnomaly {
    /// Create a new anomaly finding with the current timestamp.
    #[must_use]
    pub fn now(kind: ArchiveAnomalyKind) -> Self {
        Self {
            kind,
            detected_at: mcp_agent_mail_core::timestamps::now_micros(),
        }
    }

    /// Severity (delegated to the kind).
    #[must_use]
    pub const fn severity(&self) -> AnomalySeverity {
        self.kind.severity()
    }

    /// Remediation class (delegated to the kind).
    #[must_use]
    pub const fn remediation_class(&self) -> RemediationClass {
        self.kind.remediation_class()
    }
}

impl std::fmt::Display for ArchiveAnomaly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)
    }
}

// ============================================================================
// Anomaly report (collection of findings)
// ============================================================================

/// Aggregated archive anomaly report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveAnomalyReport {
    /// All detected anomalies, in detection order.
    pub anomalies: Vec<ArchiveAnomaly>,
}

impl ArchiveAnomalyReport {
    /// Create an empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an anomaly to the report.
    pub fn push(&mut self, anomaly: ArchiveAnomaly) {
        self.anomalies.push(anomaly);
    }

    /// Add an anomaly kind (auto-timestamped).
    pub fn record(&mut self, kind: ArchiveAnomalyKind) {
        self.push(ArchiveAnomaly::now(kind));
    }

    /// Total number of anomalies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.anomalies.len()
    }

    /// Whether the report is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anomalies.is_empty()
    }

    /// Count of anomalies at or above a given severity.
    #[must_use]
    pub fn count_at_severity(&self, min_severity: AnomalySeverity) -> usize {
        self.anomalies
            .iter()
            .filter(|a| a.severity().level() >= min_severity.level())
            .count()
    }

    /// Count of anomalies that have any automated remediation available.
    #[must_use]
    pub fn actionable_count(&self) -> usize {
        self.anomalies
            .iter()
            .filter(|a| a.remediation_class().has_automated_action())
            .count()
    }

    /// Count of anomalies that can be fixed without operator confirmation.
    #[must_use]
    pub fn safe_auto_count(&self) -> usize {
        self.anomalies
            .iter()
            .filter(|a| a.remediation_class().is_unattended())
            .count()
    }

    /// Anomalies filtered to a specific remediation class.
    #[must_use]
    pub fn by_remediation_class(&self, class: RemediationClass) -> Vec<&ArchiveAnomaly> {
        self.anomalies
            .iter()
            .filter(|a| a.remediation_class() == class)
            .collect()
    }

    /// Anomalies filtered by tag.
    #[must_use]
    pub fn by_tag(&self, tag: &str) -> Vec<&ArchiveAnomaly> {
        self.anomalies
            .iter()
            .filter(|a| a.kind.tag() == tag)
            .collect()
    }

    /// Highest severity in the report, or `None` if empty.
    #[must_use]
    pub fn max_severity(&self) -> Option<AnomalySeverity> {
        self.anomalies.iter().map(ArchiveAnomaly::severity).max()
    }

    /// Sort anomalies by severity (highest first), then by tag for stability.
    pub fn sort_by_severity(&mut self) {
        self.anomalies.sort_by(|a, b| {
            b.severity()
                .level()
                .cmp(&a.severity().level())
                .then_with(|| a.kind.tag().cmp(b.kind.tag()))
        });
    }
}

// ============================================================================
// Convenience: all known anomaly tags
// ============================================================================

/// All known anomaly tags, useful for documentation and schema validation.
pub const ALL_ANOMALY_TAGS: &[&str] = &[
    "agent_profile_mismatch",
    "archive_db_count_drift",
    "archive_db_project_mismatch",
    "duplicate_canonical_id",
    "file_reservation_artifact_mismatch",
    "incomplete_frontmatter",
    "invalid_date_directory",
    "invalid_message_id",
    "invalid_project_metadata",
    "malformed_agent_profile",
    "message_recipient_copy_mismatch",
    "missing_canonical_message",
    "missing_frontmatter",
    "missing_project_metadata",
    "orphaned_agent_profile",
    "suspicious_ephemeral_project",
    "unexpected_file_in_message_dir",
    "unexpected_symlink",
];

// ============================================================================
// Archive anomaly scanner
// ============================================================================

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Required frontmatter fields for a complete message.
const REQUIRED_FRONTMATTER_FIELDS: &[&str] = &["from", "to", "subject"];

/// Slug prefixes that suggest an ephemeral/test project.
const SUSPICIOUS_SLUG_PREFIXES: &[&str] = &["tmp-", "tmp_", "test-", "test_", "dev-", "dev_"];

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Extract the JSON frontmatter text from a `---json\n...\n---` block.
pub(crate) fn extract_json_frontmatter(content: &str) -> Option<&str> {
    let start = content.find("---json")?;
    let after_start = &content[start..];
    let json_start = if after_start.starts_with("---json\r\n") {
        start + "---json\r\n".len()
    } else if after_start.starts_with("---json\n") {
        start + "---json\n".len()
    } else {
        return None;
    };

    let mut search_from = json_start;
    while let Some(relative) = content[search_from..].find("---") {
        let marker_start = search_from + relative;
        if marker_start == 0 || !content[..marker_start].ends_with('\n') {
            search_from = marker_start + 3;
            continue;
        }

        let after_marker = marker_start + 3;
        if after_marker == content.len()
            || content[after_marker..].starts_with("\r\n")
            || content[after_marker..].starts_with('\n')
        {
            return Some(&content[json_start..marker_start]);
        }

        search_from = marker_start + 3;
    }

    None
}

/// Check whether a slug looks ephemeral or test-oriented.
fn suspicious_project_reason(slug: &str, human_key: Option<&str>) -> Option<String> {
    let lower = slug.to_ascii_lowercase();
    for prefix in SUSPICIOUS_SLUG_PREFIXES {
        if lower.starts_with(prefix) {
            return Some(format!(
                "project slug '{slug}' has ephemeral/test prefix '{prefix}'"
            ));
        }
    }

    if let Some(hk) = human_key
        && mcp_agent_mail_core::ephemeral::path_has_ephemeral_root(Path::new(hk))
    {
        return Some(format!(
            "human_key '{hk}' resolves into a temporary filesystem root"
        ));
    }

    None
}

/// Scan the archive at `storage_root` for anomalies and return a structured report.
///
/// This function walks `{storage_root}/projects/` and inspects:
///
/// - **Project metadata**: missing or malformed `project.json`, suspicious/ephemeral slugs
/// - **Agent profiles**: orphaned agents (parent project unrecognized), missing or
///   malformed `profile.json`
/// - **Message files**: duplicate canonical IDs, missing/unparseable JSON frontmatter,
///   invalid message IDs, incomplete required fields
/// - **Archive structure**: invalid year/month directory names, non-`.md` files in
///   message directories, unexpected symlinks
///
/// The scanner never modifies the archive. It only reads files and reports findings.
#[must_use]
pub fn scan_archive_anomalies(storage_root: &Path) -> ArchiveAnomalyReport {
    let mut report = ArchiveAnomalyReport::new();
    let projects_dir = storage_root.join("projects");

    if !is_real_directory(&projects_dir) {
        return report;
    }

    // Check for symlink at the projects/ level itself.
    if is_symlink(&projects_dir) {
        report.record(ArchiveAnomalyKind::UnexpectedSymlink {
            path: projects_dir.clone(),
            target: std::fs::read_link(&projects_dir).ok(),
        });
        return report;
    }

    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return report;
    };

    // Collect and sort project directories for deterministic output.
    let mut project_dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in project_entries.flatten() {
        let path = entry.path();

        // Check for symlinks at the project level.
        if is_symlink(&path) {
            report.record(ArchiveAnomalyKind::UnexpectedSymlink {
                path: path.clone(),
                target: std::fs::read_link(&path).ok(),
            });
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(slug) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        project_dirs.push((slug, path));
    }
    project_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    // Track all canonical message IDs across all projects for global dedup.
    // Maps message_id -> (first_path, [duplicate_paths]).
    let mut global_message_ids: BTreeMap<i64, (PathBuf, Vec<PathBuf>)> = BTreeMap::new();

    // Collect recognized project slugs for orphan detection.
    let recognized_slugs: BTreeSet<String> =
        project_dirs.iter().map(|(slug, _)| slug.clone()).collect();

    for (slug, project_path) in &project_dirs {
        // -- Project metadata checks --
        scan_project_metadata(&mut report, project_path, slug);

        // -- Agent profile checks --
        scan_project_agents(&mut report, project_path, slug, &recognized_slugs);

        // -- Message file checks (also populates global_message_ids) --
        scan_project_messages(&mut report, project_path, &mut global_message_ids);
    }

    // Emit duplicate canonical ID anomalies from the global map.
    for (message_id, (keep_path, duplicate_paths)) in &global_message_ids {
        if !duplicate_paths.is_empty() {
            report.record(ArchiveAnomalyKind::DuplicateCanonicalId {
                message_id: *message_id,
                keep_path: keep_path.clone(),
                duplicate_paths: duplicate_paths.clone(),
            });
        }
    }

    report
}

/// Check a single project directory for metadata anomalies.
fn scan_project_metadata(report: &mut ArchiveAnomalyReport, project_path: &Path, slug: &str) {
    let project_json_path = project_path.join("project.json");

    if !is_real_file(&project_json_path) {
        // Missing project.json.
        report.record(ArchiveAnomalyKind::MissingProjectMetadata {
            project_dir: project_path.to_path_buf(),
            fallback_slug: slug.to_string(),
        });

        // Check for suspicious slug even without metadata.
        if let Some(reason) = suspicious_project_reason(slug, None) {
            report.record(ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: project_path.to_path_buf(),
                slug: slug.to_string(),
                human_key: None,
                reason,
            });
        }
        return;
    }

    // project.json exists — try to parse it.
    let content = match std::fs::read_to_string(&project_json_path) {
        Ok(c) => c,
        Err(e) => {
            report.record(ArchiveAnomalyKind::InvalidProjectMetadata {
                path: project_json_path,
                slug: slug.to_string(),
                canonical_human_key: None,
                detail: format!("cannot read file: {e}"),
            });
            return;
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            report.record(ArchiveAnomalyKind::InvalidProjectMetadata {
                path: project_json_path,
                slug: slug.to_string(),
                canonical_human_key: None,
                detail: format!("malformed JSON: {e}"),
            });
            return;
        }
    };

    // Check required fields.
    let json_slug = parsed
        .get("slug")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let human_key = parsed
        .get("human_key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    if json_slug.is_none() && human_key.is_none() {
        report.record(ArchiveAnomalyKind::InvalidProjectMetadata {
            path: project_json_path,
            slug: slug.to_string(),
            canonical_human_key: None,
            detail: "project.json has neither 'slug' nor 'human_key' field".to_string(),
        });
    }

    // Check for suspicious project.
    if let Some(reason) = suspicious_project_reason(slug, human_key.as_deref()) {
        report.record(ArchiveAnomalyKind::SuspiciousEphemeralProject {
            project_dir: project_path.to_path_buf(),
            slug: slug.to_string(),
            human_key,
            reason,
        });
    }
}

/// Check agent profile directories under a project.
fn scan_project_agents(
    report: &mut ArchiveAnomalyReport,
    project_path: &Path,
    _project_slug: &str,
    _recognized_slugs: &BTreeSet<String>,
) {
    let agents_dir = project_path.join("agents");
    if !is_real_directory(&agents_dir) {
        return;
    }

    if is_symlink(&agents_dir) {
        report.record(ArchiveAnomalyKind::UnexpectedSymlink {
            path: agents_dir.clone(),
            target: std::fs::read_link(&agents_dir).ok(),
        });
        return;
    }

    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return;
    };

    let mut agent_dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();

        if is_symlink(&path) {
            report.record(ArchiveAnomalyKind::UnexpectedSymlink {
                path: path.clone(),
                target: std::fs::read_link(&path).ok(),
            });
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(agent_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        agent_dirs.push((agent_name, path));
    }
    agent_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    for (agent_name, agent_path) in &agent_dirs {
        let profile_path = agent_path.join("profile.json");

        if !is_real_file(&profile_path) {
            // Agent directory exists but no profile.json.
            report.record(ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path: profile_path.clone(),
                agent_name: agent_name.clone(),
                detail: "profile.json is missing".to_string(),
            });
            continue;
        }

        // Try to read and parse the profile.
        let content = match std::fs::read_to_string(&profile_path) {
            Ok(c) => c,
            Err(e) => {
                report.record(ArchiveAnomalyKind::MalformedAgentProfile {
                    profile_path: profile_path.clone(),
                    agent_name: agent_name.clone(),
                    detail: format!("cannot read file: {e}"),
                });
                continue;
            }
        };

        if let Err(e) = serde_json::from_str::<serde_json::Value>(&content) {
            report.record(ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path: profile_path.clone(),
                agent_name: agent_name.clone(),
                detail: format!("malformed JSON: {e}"),
            });
        }
    }
}

/// Scan message files under a project, checking for frontmatter anomalies and
/// populating the global duplicate-ID map.
fn scan_project_messages(
    report: &mut ArchiveAnomalyReport,
    project_path: &Path,
    global_message_ids: &mut BTreeMap<i64, (PathBuf, Vec<PathBuf>)>,
) {
    let messages_dir = project_path.join("messages");
    if !is_real_directory(&messages_dir) {
        return;
    }

    if is_symlink(&messages_dir) {
        report.record(ArchiveAnomalyKind::UnexpectedSymlink {
            path: messages_dir.clone(),
            target: std::fs::read_link(&messages_dir).ok(),
        });
        return;
    }

    let Ok(year_entries) = std::fs::read_dir(&messages_dir) else {
        return;
    };

    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();

        if is_symlink(&year_path) {
            report.record(ArchiveAnomalyKind::UnexpectedSymlink {
                path: year_path.clone(),
                target: std::fs::read_link(&year_path).ok(),
            });
            continue;
        }

        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() {
            continue;
        }

        let Some(year_name) = year_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        // Validate year directory name: must be exactly 4 ASCII digits.
        if year_name.len() != 4 || !year_name.bytes().all(|b| b.is_ascii_digit()) {
            report.record(ArchiveAnomalyKind::InvalidDateDirectory {
                path: year_path.clone(),
                level: DateDirectoryLevel::Year,
                name: year_name.to_string(),
            });
            continue;
        }

        let Ok(month_entries) = std::fs::read_dir(&year_path) else {
            continue;
        };

        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();

            if is_symlink(&month_path) {
                report.record(ArchiveAnomalyKind::UnexpectedSymlink {
                    path: month_path.clone(),
                    target: std::fs::read_link(&month_path).ok(),
                });
                continue;
            }

            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() {
                continue;
            }

            let Some(month_name) = month_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            // Validate month directory name: must be exactly 2 ASCII digits.
            if month_name.len() != 2 || !month_name.bytes().all(|b| b.is_ascii_digit()) {
                report.record(ArchiveAnomalyKind::InvalidDateDirectory {
                    path: month_path.clone(),
                    level: DateDirectoryLevel::Month,
                    name: month_name.to_string(),
                });
                continue;
            }

            let Ok(file_entries) = std::fs::read_dir(&month_path) else {
                continue;
            };

            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();

                if is_symlink(&file_path) {
                    report.record(ArchiveAnomalyKind::UnexpectedSymlink {
                        path: file_path.clone(),
                        target: std::fs::read_link(&file_path).ok(),
                    });
                    continue;
                }

                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }

                // Check for non-.md files.
                if file_path.extension().is_none_or(|ext| ext != "md") {
                    report.record(ArchiveAnomalyKind::UnexpectedFileInMessageDir {
                        path: file_path.clone(),
                    });
                    continue;
                }

                // Parse and validate frontmatter.
                scan_message_file(report, &file_path, global_message_ids);
            }
        }
    }
}

/// Scan a single `.md` message file for frontmatter anomalies.
fn scan_message_file(
    report: &mut ArchiveAnomalyReport,
    file_path: &Path,
    global_message_ids: &mut BTreeMap<i64, (PathBuf, Vec<PathBuf>)>,
) {
    let Ok(content) = std::fs::read_to_string(file_path) else {
        // Unreadable file — treat as missing frontmatter.
        report.record(ArchiveAnomalyKind::MissingFrontmatter {
            path: file_path.to_path_buf(),
        });
        return;
    };

    let Some(frontmatter_text) = extract_json_frontmatter(&content) else {
        report.record(ArchiveAnomalyKind::MissingFrontmatter {
            path: file_path.to_path_buf(),
        });
        return;
    };

    let parsed: serde_json::Value = match serde_json::from_str(frontmatter_text) {
        Ok(v) => v,
        Err(e) => {
            report.record(ArchiveAnomalyKind::UnparseableFrontmatter {
                path: file_path.to_path_buf(),
                parse_error: e.to_string(),
            });
            return;
        }
    };

    // Validate message ID.
    let message_id = match parsed.get("id") {
        None => {
            report.record(ArchiveAnomalyKind::InvalidMessageId {
                path: file_path.to_path_buf(),
                detail: "missing 'id' field".to_string(),
            });
            return;
        }
        Some(id_value) => match id_value.as_i64() {
            None => {
                report.record(ArchiveAnomalyKind::InvalidMessageId {
                    path: file_path.to_path_buf(),
                    detail: format!("'id' is not a valid integer: {id_value}"),
                });
                return;
            }
            Some(id) if id <= 0 => {
                report.record(ArchiveAnomalyKind::InvalidMessageId {
                    path: file_path.to_path_buf(),
                    detail: format!("'id' is not positive: {id}"),
                });
                return;
            }
            Some(id) => id,
        },
    };

    // Track duplicate canonical message IDs.
    global_message_ids
        .entry(message_id)
        .and_modify(|(_keep, dupes)| {
            dupes.push(file_path.to_path_buf());
        })
        .or_insert_with(|| (file_path.to_path_buf(), Vec::new()));

    // Check required frontmatter fields.
    let mut missing_fields: Vec<String> = REQUIRED_FRONTMATTER_FIELDS
        .iter()
        .filter(|&&field| {
            let value = parsed.get(field);
            match value {
                None => true,
                Some(serde_json::Value::Null) => true,
                Some(serde_json::Value::String(s)) => s.trim().is_empty(),
                Some(serde_json::Value::Array(arr)) => arr.is_empty(),
                _ => false,
            }
        })
        .map(|&field| field.to_string())
        .collect();
    if !message_frontmatter_has_created_timestamp(&parsed) {
        missing_fields.push("created|created_at|created_ts".to_string());
    }

    if !missing_fields.is_empty() {
        report.record(ArchiveAnomalyKind::IncompleteFrontmatter {
            path: file_path.to_path_buf(),
            missing_fields,
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ArchiveAgentKey {
    project_slug: String,
    agent_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ArchiveMessageCopyKey {
    project_slug: String,
    agent_name: String,
    mailbox: MailboxCopyKind,
    message_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ArchiveReservationKey {
    project_slug: String,
    reservation_id: i64,
}

#[derive(Debug, Clone)]
struct ArchiveJsonArtifact {
    path: PathBuf,
    json: Option<serde_json::Value>,
    parse_error: Option<String>,
}

#[derive(Debug, Default)]
struct ArchiveDbArtifactIndex {
    canonical_messages: BTreeMap<i64, Vec<PathBuf>>,
    mailbox_copies: BTreeMap<ArchiveMessageCopyKey, Vec<PathBuf>>,
    agent_profiles: BTreeMap<ArchiveAgentKey, ArchiveJsonArtifact>,
    reservation_artifacts: BTreeMap<ArchiveReservationKey, ArchiveJsonArtifact>,
}

#[derive(Debug)]
struct DbMessageArtifactExpectation {
    project_slug: String,
    message_id: i64,
    sender_name: String,
    subject: String,
}

#[derive(Debug)]
struct DbMailboxCopyExpectation {
    project_slug: String,
    message_id: i64,
    agent_name: String,
    mailbox: MailboxCopyKind,
    detail: String,
}

#[derive(Debug)]
struct DbAgentProfileExpectation {
    project_slug: String,
    agent_name: String,
    program: String,
    model: String,
}

#[derive(Debug)]
struct DbReservationArtifactExpectation {
    project_slug: String,
    reservation_id: i64,
    agent_name: String,
    path_pattern: String,
}

fn is_archive_year_component(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_archive_month_component(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn read_json_file_artifact(path: &Path) -> ArchiveJsonArtifact {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(json) => ArchiveJsonArtifact {
                path: path.to_path_buf(),
                json: Some(json),
                parse_error: None,
            },
            Err(error) => ArchiveJsonArtifact {
                path: path.to_path_buf(),
                json: None,
                parse_error: Some(error.to_string()),
            },
        },
        Err(error) => ArchiveJsonArtifact {
            path: path.to_path_buf(),
            json: None,
            parse_error: Some(error.to_string()),
        },
    }
}

fn archive_message_id_from_file(path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    let frontmatter = extract_json_frontmatter(&content)?;
    let parsed = serde_json::from_str::<serde_json::Value>(frontmatter).ok()?;
    parsed
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
}

fn message_frontmatter_has_created_timestamp(parsed: &serde_json::Value) -> bool {
    ["created", "created_at", "created_ts"].iter().any(|field| {
        parsed.get(field).is_some_and(|value| match value {
            serde_json::Value::Null => false,
            serde_json::Value::String(text) => !text.trim().is_empty(),
            serde_json::Value::Number(number) => number.as_i64().is_some_and(|raw| raw > 0),
            _ => true,
        })
    })
}

fn collect_dated_message_files(root: &Path) -> Vec<PathBuf> {
    if !is_real_directory(root) || is_symlink(root) {
        return Vec::new();
    }

    let Ok(year_entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();
        if is_symlink(&year_path) || !year_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(year_name) = year_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_archive_year_component(year_name) {
            continue;
        }

        let Ok(month_entries) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();
            if is_symlink(&month_path) || !month_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(month_name) = month_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !is_archive_month_component(month_name) {
                continue;
            }

            let Ok(file_entries) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();
                if is_symlink(&file_path)
                    || !file_entry.file_type().is_ok_and(|kind| kind.is_file())
                    || file_path.extension().is_none_or(|ext| ext != "md")
                {
                    continue;
                }
                files.push(file_path);
            }
        }
    }
    files.sort();
    files
}

fn collect_project_canonical_messages(index: &mut ArchiveDbArtifactIndex, project_path: &Path) {
    for file_path in collect_dated_message_files(&project_path.join("messages")) {
        if let Some(message_id) = archive_message_id_from_file(&file_path) {
            index
                .canonical_messages
                .entry(message_id)
                .or_default()
                .push(file_path);
        }
    }
}

fn collect_project_agent_artifacts(
    index: &mut ArchiveDbArtifactIndex,
    project_slug: &str,
    project_path: &Path,
) {
    let agents_dir = project_path.join("agents");
    if !is_real_directory(&agents_dir) || is_symlink(&agents_dir) {
        return;
    }

    let Ok(agent_entries) = std::fs::read_dir(&agents_dir) else {
        return;
    };
    for agent_entry in agent_entries.flatten() {
        let agent_path = agent_entry.path();
        if is_symlink(&agent_path) || !agent_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(agent_name) = agent_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        let profile_path = agent_path.join("profile.json");
        if is_real_file(&profile_path) {
            index.agent_profiles.insert(
                ArchiveAgentKey {
                    project_slug: project_slug.to_string(),
                    agent_name: agent_name.clone(),
                },
                read_json_file_artifact(&profile_path),
            );
        }

        for (mailbox, mailbox_dir_name) in [
            (MailboxCopyKind::Inbox, "inbox"),
            (MailboxCopyKind::Outbox, "outbox"),
        ] {
            let mailbox_dir = agent_path.join(mailbox_dir_name);
            for file_path in collect_dated_message_files(&mailbox_dir) {
                if let Some(message_id) = archive_message_id_from_file(&file_path) {
                    index
                        .mailbox_copies
                        .entry(ArchiveMessageCopyKey {
                            project_slug: project_slug.to_string(),
                            agent_name: agent_name.clone(),
                            mailbox,
                            message_id,
                        })
                        .or_default()
                        .push(file_path);
                }
            }
        }
    }
}

fn collect_project_reservation_artifacts(
    index: &mut ArchiveDbArtifactIndex,
    project_slug: &str,
    project_path: &Path,
) {
    let reservation_dir = project_path.join("file_reservations");
    if !is_real_directory(&reservation_dir) || is_symlink(&reservation_dir) {
        return;
    }

    let Ok(entries) = std::fs::read_dir(&reservation_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_symlink(&path)
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
            || path.extension().is_none_or(|ext| ext != "json")
        {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(raw_id) = file_name
            .strip_prefix("id-")
            .and_then(|rest| rest.strip_suffix(".json"))
        else {
            continue;
        };
        let Ok(reservation_id) = raw_id.parse::<i64>() else {
            continue;
        };
        if reservation_id <= 0 {
            continue;
        }
        index.reservation_artifacts.insert(
            ArchiveReservationKey {
                project_slug: project_slug.to_string(),
                reservation_id,
            },
            read_json_file_artifact(&path),
        );
    }
}

fn collect_archive_db_artifact_index(storage_root: &Path) -> ArchiveDbArtifactIndex {
    let projects_dir = storage_root.join("projects");
    let mut index = ArchiveDbArtifactIndex::default();
    if !is_real_directory(&projects_dir) || is_symlink(&projects_dir) {
        return index;
    }

    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return index;
    };
    let mut project_dirs = Vec::new();
    for entry in project_entries.flatten() {
        let path = entry.path();
        if is_symlink(&path) || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(slug) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        project_dirs.push((slug, path));
    }
    project_dirs.sort_by(|left, right| left.0.cmp(&right.0));

    for (project_slug, project_path) in project_dirs {
        collect_project_canonical_messages(&mut index, &project_path);
        collect_project_agent_artifacts(&mut index, &project_slug, &project_path);
        collect_project_reservation_artifacts(&mut index, &project_slug, &project_path);
    }

    index
}

fn open_canonical_db_for_archive_verifier(db_path: &Path) -> Option<crate::CanonicalDbConn> {
    if db_path.as_os_str() == ":memory:" {
        return None;
    }
    crate::pool::validate_sqlite_target_path(db_path, "archive verifier sqlite target").ok()?;
    let metadata = std::fs::symlink_metadata(db_path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    crate::CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).ok()
}

fn query_db_message_expectations(
    conn: &crate::CanonicalDbConn,
) -> Result<Vec<DbMessageArtifactExpectation>, String> {
    let rows = conn
        .query_sync(
            "SELECT m.id AS message_id,
                    p.slug AS project_slug,
                    a.name AS sender_name,
                    m.subject AS subject
             FROM messages m
             JOIN projects p ON p.id = m.project_id
             JOIN agents a ON a.id = m.sender_id
             ORDER BY p.slug, m.id",
            &[],
        )
        .map_err(|error| error.to_string())?;

    rows.into_iter()
        .map(|row| {
            Ok(DbMessageArtifactExpectation {
                project_slug: row
                    .get_named::<String>("project_slug")
                    .map_err(|error| error.to_string())?,
                message_id: row
                    .get_named::<i64>("message_id")
                    .map_err(|error| error.to_string())?,
                sender_name: row
                    .get_named::<String>("sender_name")
                    .map_err(|error| error.to_string())?,
                subject: row
                    .get_named::<String>("subject")
                    .map_err(|error| error.to_string())?,
            })
        })
        .collect()
}

fn query_db_mailbox_copy_expectations(
    conn: &crate::CanonicalDbConn,
) -> Result<Vec<DbMailboxCopyExpectation>, String> {
    let mut expectations = Vec::new();

    let sender_rows = conn
        .query_sync(
            "SELECT m.id AS message_id,
                    p.slug AS project_slug,
                    sender.name AS agent_name
             FROM messages m
             JOIN projects p ON p.id = m.project_id
             JOIN agents sender ON sender.id = m.sender_id
             ORDER BY p.slug, m.id, sender.name",
            &[],
        )
        .map_err(|error| error.to_string())?;
    for row in sender_rows {
        expectations.push(DbMailboxCopyExpectation {
            project_slug: row
                .get_named::<String>("project_slug")
                .map_err(|error| error.to_string())?,
            message_id: row
                .get_named::<i64>("message_id")
                .map_err(|error| error.to_string())?,
            agent_name: row
                .get_named::<String>("agent_name")
                .map_err(|error| error.to_string())?,
            mailbox: MailboxCopyKind::Outbox,
            detail: "sender outbox copy is missing".to_string(),
        });
    }

    let recipient_rows = conn
        .query_sync(
            "SELECT m.id AS message_id,
                    p.slug AS project_slug,
                    recipient.name AS agent_name,
                    mr.kind AS recipient_kind
             FROM message_recipients mr
             JOIN messages m ON m.id = mr.message_id
             JOIN projects p ON p.id = m.project_id
             JOIN agents recipient ON recipient.id = mr.agent_id
             ORDER BY p.slug, m.id, recipient.name",
            &[],
        )
        .map_err(|error| error.to_string())?;
    for row in recipient_rows {
        let recipient_kind = row
            .get_named::<String>("recipient_kind")
            .unwrap_or_else(|_| "to".to_string());
        expectations.push(DbMailboxCopyExpectation {
            project_slug: row
                .get_named::<String>("project_slug")
                .map_err(|error| error.to_string())?,
            message_id: row
                .get_named::<i64>("message_id")
                .map_err(|error| error.to_string())?,
            agent_name: row
                .get_named::<String>("agent_name")
                .map_err(|error| error.to_string())?,
            mailbox: MailboxCopyKind::Inbox,
            detail: format!("recipient {recipient_kind} inbox copy is missing"),
        });
    }

    Ok(expectations)
}

fn query_db_agent_profile_expectations(
    conn: &crate::CanonicalDbConn,
) -> Result<Vec<DbAgentProfileExpectation>, String> {
    let rows = conn
        .query_sync(
            "SELECT p.slug AS project_slug,
                    a.name AS agent_name,
                    a.program AS program,
                    a.model AS model
             FROM agents a
             JOIN projects p ON p.id = a.project_id
             ORDER BY p.slug, a.name",
            &[],
        )
        .map_err(|error| error.to_string())?;

    rows.into_iter()
        .map(|row| {
            Ok(DbAgentProfileExpectation {
                project_slug: row
                    .get_named::<String>("project_slug")
                    .map_err(|error| error.to_string())?,
                agent_name: row
                    .get_named::<String>("agent_name")
                    .map_err(|error| error.to_string())?,
                program: row
                    .get_named::<String>("program")
                    .map_err(|error| error.to_string())?,
                model: row
                    .get_named::<String>("model")
                    .map_err(|error| error.to_string())?,
            })
        })
        .collect()
}

fn query_db_reservation_expectations(
    conn: &crate::CanonicalDbConn,
) -> Result<Vec<DbReservationArtifactExpectation>, String> {
    let rows = conn
        .query_sync(
            "SELECT fr.id AS reservation_id,
                    p.slug AS project_slug,
                    a.name AS agent_name,
                    fr.path_pattern AS path_pattern
             FROM file_reservations fr
             JOIN projects p ON p.id = fr.project_id
             JOIN agents a ON a.id = fr.agent_id
             ORDER BY p.slug, fr.id",
            &[],
        )
        .map_err(|error| error.to_string())?;

    rows.into_iter()
        .map(|row| {
            Ok(DbReservationArtifactExpectation {
                project_slug: row
                    .get_named::<String>("project_slug")
                    .map_err(|error| error.to_string())?,
                reservation_id: row
                    .get_named::<i64>("reservation_id")
                    .map_err(|error| error.to_string())?,
                agent_name: row
                    .get_named::<String>("agent_name")
                    .map_err(|error| error.to_string())?,
                path_pattern: row
                    .get_named::<String>("path_pattern")
                    .map_err(|error| error.to_string())?,
            })
        })
        .collect()
}

fn json_string_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn profile_mismatch_detail(
    expected: &DbAgentProfileExpectation,
    artifact: &ArchiveJsonArtifact,
) -> Option<String> {
    if let Some(error) = &artifact.parse_error {
        return Some(format!("profile.json is not valid JSON: {error}"));
    }
    let Some(json) = &artifact.json else {
        return Some("profile.json could not be parsed".to_string());
    };

    let mut mismatches = Vec::new();
    for (field, expected_value) in [
        ("name", expected.agent_name.as_str()),
        ("program", expected.program.as_str()),
        ("model", expected.model.as_str()),
    ] {
        match json_string_field(json, field) {
            Some(actual) if actual == expected_value => {}
            Some(actual) => {
                mismatches.push(format!("{field} archive='{actual}' db='{expected_value}'"));
            }
            None => mismatches.push(format!("{field} missing in archive profile")),
        }
    }

    (!mismatches.is_empty()).then(|| mismatches.join("; "))
}

fn reservation_mismatch_detail(
    expected: &DbReservationArtifactExpectation,
    artifact: &ArchiveJsonArtifact,
) -> Option<String> {
    if let Some(error) = &artifact.parse_error {
        return Some(format!("reservation artifact is not valid JSON: {error}"));
    }
    let Some(json) = &artifact.json else {
        return Some("reservation artifact could not be parsed".to_string());
    };

    let mut mismatches = Vec::new();
    match json.get("id").and_then(serde_json::Value::as_i64) {
        Some(actual) if actual == expected.reservation_id => {}
        Some(actual) => mismatches.push(format!(
            "id archive={actual} db={}",
            expected.reservation_id
        )),
        None => mismatches.push("id missing in archive reservation".to_string()),
    }

    match json_string_field(json, "agent") {
        Some(actual) if actual == expected.agent_name => {}
        Some(actual) => mismatches.push(format!(
            "agent archive='{actual}' db='{}'",
            expected.agent_name
        )),
        None => mismatches.push("agent missing in archive reservation".to_string()),
    }

    let archive_path_pattern =
        json_string_field(json, "path_pattern").or_else(|| json_string_field(json, "path"));
    match archive_path_pattern {
        Some(actual) if actual == expected.path_pattern => {}
        Some(actual) => mismatches.push(format!(
            "path_pattern archive='{actual}' db='{}'",
            expected.path_pattern
        )),
        None => mismatches.push("path_pattern missing in archive reservation".to_string()),
    }

    (!mismatches.is_empty()).then(|| mismatches.join("; "))
}

fn append_archive_db_artifact_cross_checks(
    report: &mut ArchiveAnomalyReport,
    storage_root: &Path,
    db_path: &Path,
) {
    let Some(conn) = open_canonical_db_for_archive_verifier(db_path) else {
        return;
    };
    let index = collect_archive_db_artifact_index(storage_root);

    let mut messages_missing_canonical = HashSet::new();
    if let Ok(messages) = query_db_message_expectations(&conn) {
        for message in messages {
            if !index.canonical_messages.contains_key(&message.message_id) {
                messages_missing_canonical.insert(message.message_id);
                report.record(ArchiveAnomalyKind::MissingCanonicalMessage {
                    project_slug: message.project_slug,
                    message_id: message.message_id,
                    db_subject: message.subject,
                    db_sender: message.sender_name,
                });
            }
        }
    }

    if let Ok(mailbox_copies) = query_db_mailbox_copy_expectations(&conn) {
        for expected in mailbox_copies {
            if messages_missing_canonical.contains(&expected.message_id) {
                continue;
            }
            let key = ArchiveMessageCopyKey {
                project_slug: expected.project_slug.clone(),
                agent_name: expected.agent_name.clone(),
                mailbox: expected.mailbox,
                message_id: expected.message_id,
            };
            if !index.mailbox_copies.contains_key(&key) {
                report.record(ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                    project_slug: expected.project_slug,
                    message_id: expected.message_id,
                    agent_name: expected.agent_name,
                    mailbox: expected.mailbox,
                    detail: expected.detail,
                });
            }
        }
    }

    if let Ok(agent_profiles) = query_db_agent_profile_expectations(&conn) {
        for expected in agent_profiles {
            let key = ArchiveAgentKey {
                project_slug: expected.project_slug.clone(),
                agent_name: expected.agent_name.clone(),
            };
            let expected_path = storage_root
                .join("projects")
                .join(&expected.project_slug)
                .join("agents")
                .join(&expected.agent_name)
                .join("profile.json");
            match index.agent_profiles.get(&key) {
                Some(artifact) => {
                    if let Some(detail) = profile_mismatch_detail(&expected, artifact) {
                        report.record(ArchiveAnomalyKind::AgentProfileMismatch {
                            project_slug: expected.project_slug,
                            agent_name: expected.agent_name,
                            profile_path: artifact.path.clone(),
                            detail,
                        });
                    }
                }
                None => report.record(ArchiveAnomalyKind::AgentProfileMismatch {
                    project_slug: expected.project_slug,
                    agent_name: expected.agent_name,
                    profile_path: expected_path,
                    detail: "profile.json is missing".to_string(),
                }),
            }
        }
    }

    if let Ok(reservations) = query_db_reservation_expectations(&conn) {
        for expected in reservations {
            let key = ArchiveReservationKey {
                project_slug: expected.project_slug.clone(),
                reservation_id: expected.reservation_id,
            };
            let expected_path = storage_root
                .join("projects")
                .join(&expected.project_slug)
                .join("file_reservations")
                .join(format!("id-{}.json", expected.reservation_id));
            match index.reservation_artifacts.get(&key) {
                Some(artifact) => {
                    if let Some(detail) = reservation_mismatch_detail(&expected, artifact) {
                        report.record(ArchiveAnomalyKind::FileReservationArtifactMismatch {
                            project_slug: expected.project_slug,
                            reservation_id: expected.reservation_id,
                            artifact_path: artifact.path.clone(),
                            detail,
                        });
                    }
                }
                None => report.record(ArchiveAnomalyKind::FileReservationArtifactMismatch {
                    project_slug: expected.project_slug,
                    reservation_id: expected.reservation_id,
                    artifact_path: expected_path,
                    detail: "stable id reservation artifact is missing".to_string(),
                }),
            }
        }
    }
}

/// Scan the archive for anomalies, also comparing against a database for
/// identity mismatches and count drift.
///
/// This extends [`scan_archive_anomalies`] with DB-side cross-checks:
/// - Projects in the archive but not in the DB (or vice versa)
/// - Message count drift between archive and DB
///
/// Requires a path to the database file. If the DB cannot be opened or
/// queried, the DB-side checks are skipped silently and the archive-only
/// anomalies are still returned.
#[must_use]
pub fn scan_archive_anomalies_with_db(storage_root: &Path, db_path: &Path) -> ArchiveAnomalyReport {
    let mut report = scan_archive_anomalies(storage_root);

    // Gather archive inventory for comparison.
    let archive_inventory = crate::reconstruct::scan_archive_message_inventory(storage_root);

    // Try to get DB inventory for cross-checks.
    let db_inventory = crate::pool::inspect_mailbox_db_inventory(db_path).ok();

    if let Some(ref inv) = db_inventory {
        // Check for archive/DB project identity mismatches.
        let missing = crate::reconstruct::archive_missing_project_identities(
            &archive_inventory,
            &inv.project_identities,
        );
        for label in missing {
            report.record(ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug: label.clone(),
                archive_human_key: None,
                detail: "archive project has no matching DB identity".to_string(),
            });
        }

        // Check for message count drift.
        let archive_count = archive_inventory.unique_message_ids;
        let db_count_result = crate::reconstruct::collect_db_message_ids(db_path);
        if let Ok(db_ids) = db_count_result {
            let db_count = db_ids.len();
            let drift = archive_count.abs_diff(db_count);
            if drift > 0 {
                report.record(ArchiveAnomalyKind::ArchiveDbCountDrift {
                    archive_count,
                    db_count,
                    drift,
                });
            }
        }
    }

    append_archive_db_artifact_cross_checks(&mut report, storage_root, db_path);

    // Sort by severity for consistent output.
    report.sort_by_severity();
    report
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(AnomalySeverity::Info < AnomalySeverity::Warning);
        assert!(AnomalySeverity::Warning < AnomalySeverity::Error);
        assert!(AnomalySeverity::Error < AnomalySeverity::Critical);
    }

    #[test]
    fn remediation_ordering() {
        assert!(RemediationClass::ReportOnly < RemediationClass::SafeAuto);
        assert!(RemediationClass::SafeAuto < RemediationClass::NeedsConfirmation);
        assert!(RemediationClass::NeedsConfirmation < RemediationClass::ManualOnly);
    }

    #[test]
    fn duplicate_canonical_id_classification() {
        let kind = ArchiveAnomalyKind::DuplicateCanonicalId {
            message_id: 42,
            keep_path: PathBuf::from("/archive/messages/2026/01/msg_42.md"),
            duplicate_paths: vec![PathBuf::from("/archive/messages/2026/02/msg_42.md")],
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(
            kind.remediation_class(),
            RemediationClass::NeedsConfirmation
        );
        assert_eq!(kind.tag(), "duplicate_canonical_id");
        assert!(kind.summary().contains("42"));
    }

    #[test]
    fn missing_frontmatter_classification() {
        let kind = ArchiveAnomalyKind::MissingFrontmatter {
            path: PathBuf::from("/archive/messages/2026/01/orphan.md"),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
        assert_eq!(kind.tag(), "missing_frontmatter");
    }

    #[test]
    fn unparseable_frontmatter_classification() {
        let kind = ArchiveAnomalyKind::UnparseableFrontmatter {
            path: PathBuf::from("/archive/messages/2026/01/bad.md"),
            parse_error: "expected comma".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
        assert_eq!(kind.tag(), "unparseable_frontmatter");
    }

    #[test]
    fn invalid_message_id_classification() {
        let kind = ArchiveAnomalyKind::InvalidMessageId {
            path: PathBuf::from("/archive/messages/2026/01/neg.md"),
            detail: "id is negative".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(kind.remediation_class(), RemediationClass::ManualOnly);
    }

    #[test]
    fn incomplete_frontmatter_classification() {
        let kind = ArchiveAnomalyKind::IncompleteFrontmatter {
            path: PathBuf::from("/archive/messages/2026/01/partial.md"),
            missing_fields: vec!["from".to_string(), "subject".to_string()],
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
    }

    #[test]
    fn missing_project_metadata_classification() {
        let kind = ArchiveAnomalyKind::MissingProjectMetadata {
            project_dir: PathBuf::from("/archive/projects/my-project"),
            fallback_slug: "my-project".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::SafeAuto);
    }

    #[test]
    fn invalid_project_metadata_with_canonical_key() {
        let kind = ArchiveAnomalyKind::InvalidProjectMetadata {
            path: PathBuf::from("/archive/projects/foo/project.json"),
            slug: "foo".to_string(),
            canonical_human_key: Some("Foo Project".to_string()),
            detail: "malformed JSON".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::SafeAuto);
    }

    #[test]
    fn invalid_project_metadata_without_canonical_key() {
        let kind = ArchiveAnomalyKind::InvalidProjectMetadata {
            path: PathBuf::from("/archive/projects/foo/project.json"),
            slug: "foo".to_string(),
            canonical_human_key: None,
            detail: "malformed JSON".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(kind.remediation_class(), RemediationClass::ManualOnly);
    }

    #[test]
    fn suspicious_ephemeral_project_classification() {
        let kind = ArchiveAnomalyKind::SuspiciousEphemeralProject {
            project_dir: PathBuf::from("/archive/projects/tmp-test"),
            slug: "tmp-test".to_string(),
            human_key: None,
            reason: "project slug 'tmp-test' looks ephemeral".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Info);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
    }

    #[test]
    fn orphaned_agent_classification() {
        let kind = ArchiveAnomalyKind::OrphanedAgentProfile {
            profile_path: PathBuf::from("/archive/projects/ghost/agents/BraveEagle/profile.json"),
            agent_name: "BraveEagle".to_string(),
            parent_project_dir: PathBuf::from("/archive/projects/ghost"),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Error);
        assert_eq!(kind.remediation_class(), RemediationClass::ManualOnly);
    }

    #[test]
    fn malformed_agent_profile_classification() {
        let kind = ArchiveAnomalyKind::MalformedAgentProfile {
            profile_path: PathBuf::from("/archive/projects/foo/agents/Bar/profile.json"),
            agent_name: "Bar".to_string(),
            detail: "file is empty".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
    }

    #[test]
    fn structural_anomalies_are_info() {
        let kinds = [
            ArchiveAnomalyKind::InvalidDateDirectory {
                path: PathBuf::from("/archive/projects/foo/messages/abcd"),
                level: DateDirectoryLevel::Year,
                name: "abcd".to_string(),
            },
            ArchiveAnomalyKind::UnexpectedFileInMessageDir {
                path: PathBuf::from("/archive/projects/foo/messages/2026/01/notes.txt"),
            },
            ArchiveAnomalyKind::UnexpectedSymlink {
                path: PathBuf::from("/archive/projects/foo/messages/link"),
                target: Some(PathBuf::from("/tmp/somewhere")),
            },
        ];
        for kind in &kinds {
            assert_eq!(
                kind.severity(),
                AnomalySeverity::Info,
                "kind: {}",
                kind.tag()
            );
            assert_eq!(
                kind.remediation_class(),
                RemediationClass::ReportOnly,
                "kind: {}",
                kind.tag()
            );
        }
    }

    #[test]
    fn archive_db_mismatch_classification() {
        let kind = ArchiveAnomalyKind::ArchiveDbProjectMismatch {
            archive_slug: "orphan-proj".to_string(),
            archive_human_key: Some("/data/orphan".to_string()),
            detail: "no matching DB project".to_string(),
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
    }

    #[test]
    fn archive_db_count_drift_classification() {
        let kind = ArchiveAnomalyKind::ArchiveDbCountDrift {
            archive_count: 150,
            db_count: 140,
            drift: 10,
        };
        assert_eq!(kind.severity(), AnomalySeverity::Warning);
        assert_eq!(kind.remediation_class(), RemediationClass::ReportOnly);
    }

    #[test]
    fn archive_db_artifact_drift_classification() {
        let missing = ArchiveAnomalyKind::MissingCanonicalMessage {
            project_slug: "demo-project".to_string(),
            message_id: 42,
            db_subject: "Hello".to_string(),
            db_sender: "BlueLake".to_string(),
        };
        assert_eq!(missing.severity(), AnomalySeverity::Error);
        assert_eq!(missing.remediation_class(), RemediationClass::ManualOnly);
        assert_eq!(missing.tag(), "missing_canonical_message");

        let copy = ArchiveAnomalyKind::MessageRecipientCopyMismatch {
            project_slug: "demo-project".to_string(),
            message_id: 42,
            agent_name: "GreenField".to_string(),
            mailbox: MailboxCopyKind::Inbox,
            detail: "recipient to inbox copy is missing".to_string(),
        };
        assert_eq!(copy.severity(), AnomalySeverity::Warning);
        assert_eq!(copy.remediation_class(), RemediationClass::ReportOnly);
        assert_eq!(copy.tag(), "message_recipient_copy_mismatch");

        let profile = ArchiveAnomalyKind::AgentProfileMismatch {
            project_slug: "demo-project".to_string(),
            agent_name: "BlueLake".to_string(),
            profile_path: PathBuf::from("/archive/profile.json"),
            detail: "model differs".to_string(),
        };
        assert_eq!(profile.severity(), AnomalySeverity::Warning);
        assert_eq!(profile.remediation_class(), RemediationClass::ReportOnly);
        assert_eq!(profile.tag(), "agent_profile_mismatch");

        let reservation = ArchiveAnomalyKind::FileReservationArtifactMismatch {
            project_slug: "demo-project".to_string(),
            reservation_id: 99,
            artifact_path: PathBuf::from("/archive/id-99.json"),
            detail: "path_pattern differs".to_string(),
        };
        assert_eq!(reservation.severity(), AnomalySeverity::Warning);
        assert_eq!(
            reservation.remediation_class(),
            RemediationClass::ReportOnly
        );
        assert_eq!(reservation.tag(), "file_reservation_artifact_mismatch");
    }

    fn init_archive_verifier_db(db_path: &Path) -> crate::CanonicalDbConn {
        let conn =
            crate::CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .expect("init schema");
        conn
    }

    fn write_archive_message(path: &Path, id: i64, from: &str, to: &[&str], subject: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create message parent");
        }
        let payload = serde_json::json!({
            "id": id,
            "from": from,
            "to": to,
            "cc": [],
            "bcc": [],
            "subject": subject,
            "created_at": "2026-05-10T12:00:00Z",
        });
        std::fs::write(
            path,
            format!(
                "---json\n{}\n---\nbody\n",
                serde_json::to_string(&payload).expect("serialize message")
            ),
        )
        .expect("write archive message");
    }

    #[test]
    fn scan_archive_anomalies_with_db_reports_artifact_cross_checks() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("mail.sqlite3");
        let conn = init_archive_verifier_db(&db_path);

        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'demo-project', '/data/projects/demo-project', 0);
             INSERT INTO agents (
                id, project_id, name, program, model, task_description,
                inception_ts, last_active_ts, attachments_policy, contact_policy
             ) VALUES
                (10, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto'),
                (11, 1, 'GreenField', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto'),
                (12, 1, 'SilverStone', 'codex-cli', 'gpt-5', 'holder', 0, 0, 'auto', 'auto');
             INSERT INTO messages (
                id, project_id, sender_id, thread_id, subject, body_md,
                importance, ack_required, created_ts, recipients_json, attachments
             ) VALUES
                (7, 1, 10, 'thread-7', 'DB only', 'body', 'normal', 0, 0, '{}', '[]'),
                (8, 1, 10, 'thread-8', 'Archived', 'body', 'normal', 0, 0, '{}', '[]');
             INSERT INTO message_recipients (message_id, agent_id, kind)
             VALUES (8, 11, 'to');
             INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, exclusive, reason,
                created_ts, expires_ts, released_ts
             ) VALUES (23, 1, 12, 'src/lib.rs', 1, 'verify', 0, 999999999, NULL);",
        )
        .expect("seed db");

        let project_dir = storage_root.join("projects").join("demo-project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"demo-project","human_key":"/data/projects/demo-project"}"#,
        )
        .expect("write project metadata");
        std::fs::create_dir_all(project_dir.join("agents").join("BlueLake").join("outbox"))
            .expect("create outbox");
        std::fs::create_dir_all(project_dir.join("agents").join("GreenField"))
            .expect("create recipient agent");
        std::fs::create_dir_all(project_dir.join("agents").join("SilverStone"))
            .expect("create reservation agent");

        std::fs::write(
            project_dir
                .join("agents")
                .join("BlueLake")
                .join("profile.json"),
            r#"{"name":"BlueLake","program":"wrong-cli","model":"gpt-5"}"#,
        )
        .expect("write mismatched sender profile");
        std::fs::write(
            project_dir
                .join("agents")
                .join("GreenField")
                .join("profile.json"),
            r#"{"name":"GreenField","program":"codex-cli","model":"gpt-5"}"#,
        )
        .expect("write recipient profile");
        std::fs::write(
            project_dir
                .join("agents")
                .join("SilverStone")
                .join("profile.json"),
            r#"{"name":"SilverStone","program":"codex-cli","model":"gpt-5"}"#,
        )
        .expect("write reservation holder profile");

        let canonical_dir = project_dir.join("messages").join("2026").join("05");
        write_archive_message(
            &canonical_dir.join("2026-05-10T12-00-00Z__archived__8.md"),
            8,
            "BlueLake",
            &["GreenField"],
            "Archived",
        );
        write_archive_message(
            &canonical_dir.join("2026-05-10T12-01-00Z__duplicate__8.md"),
            8,
            "BlueLake",
            &["GreenField"],
            "Archived duplicate",
        );
        write_archive_message(
            &project_dir
                .join("agents")
                .join("BlueLake")
                .join("outbox")
                .join("2026")
                .join("05")
                .join("2026-05-10T12-00-00Z__archived__8.md"),
            8,
            "BlueLake",
            &["GreenField"],
            "Archived",
        );

        let reservation_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&reservation_dir).expect("create reservation dir");
        std::fs::write(
            reservation_dir.join("id-23.json"),
            r#"{"id":23,"agent":"SilverStone","path_pattern":"src/main.rs","exclusive":true}"#,
        )
        .expect("write mismatched reservation artifact");

        let report = scan_archive_anomalies_with_db(&storage_root, &db_path);

        assert_eq!(report.by_tag("missing_canonical_message").len(), 1);
        assert_eq!(report.by_tag("duplicate_canonical_id").len(), 1);
        assert_eq!(report.by_tag("message_recipient_copy_mismatch").len(), 1);
        assert_eq!(report.by_tag("agent_profile_mismatch").len(), 1);
        assert_eq!(report.by_tag("file_reservation_artifact_mismatch").len(), 1);
    }

    #[test]
    fn report_aggregation() {
        let mut report = ArchiveAnomalyReport::new();
        assert!(report.is_empty());
        assert_eq!(report.max_severity(), None);

        report.record(ArchiveAnomalyKind::MissingProjectMetadata {
            project_dir: PathBuf::from("/a"),
            fallback_slug: "a".to_string(),
        });
        report.record(ArchiveAnomalyKind::DuplicateCanonicalId {
            message_id: 1,
            keep_path: PathBuf::from("/k"),
            duplicate_paths: vec![PathBuf::from("/d")],
        });
        report.record(ArchiveAnomalyKind::SuspiciousEphemeralProject {
            project_dir: PathBuf::from("/tmp"),
            slug: "tmp-x".to_string(),
            human_key: None,
            reason: "ephemeral".to_string(),
        });

        assert_eq!(report.len(), 3);
        assert_eq!(report.max_severity(), Some(AnomalySeverity::Error));
        assert_eq!(report.count_at_severity(AnomalySeverity::Error), 1);
        assert_eq!(report.count_at_severity(AnomalySeverity::Warning), 2);
        assert_eq!(report.actionable_count(), 2); // safe_auto + needs_confirmation
        assert_eq!(report.safe_auto_count(), 1); // missing_project_metadata
        assert_eq!(report.by_tag("duplicate_canonical_id").len(), 1);
    }

    #[test]
    fn report_sort_by_severity() {
        let mut report = ArchiveAnomalyReport::new();
        report.record(ArchiveAnomalyKind::SuspiciousEphemeralProject {
            project_dir: PathBuf::from("/tmp"),
            slug: "tmp-x".to_string(),
            human_key: None,
            reason: "ephemeral".to_string(),
        });
        report.record(ArchiveAnomalyKind::DuplicateCanonicalId {
            message_id: 1,
            keep_path: PathBuf::from("/k"),
            duplicate_paths: vec![PathBuf::from("/d")],
        });
        report.record(ArchiveAnomalyKind::MissingProjectMetadata {
            project_dir: PathBuf::from("/a"),
            fallback_slug: "a".to_string(),
        });

        report.sort_by_severity();

        assert_eq!(report.anomalies[0].severity(), AnomalySeverity::Error);
        assert_eq!(report.anomalies[1].severity(), AnomalySeverity::Warning);
        assert_eq!(report.anomalies[2].severity(), AnomalySeverity::Info);
    }

    #[test]
    fn all_anomaly_tags_sorted_and_complete() {
        // Verify the constant is sorted.
        let mut sorted = ALL_ANOMALY_TAGS.to_vec();
        sorted.sort_unstable();
        assert_eq!(ALL_ANOMALY_TAGS, sorted.as_slice());

        // Verify every tag is represented.
        assert_eq!(ALL_ANOMALY_TAGS.len(), 18);
    }

    #[test]
    fn display_formats_include_severity() {
        let kind = ArchiveAnomalyKind::MissingFrontmatter {
            path: PathBuf::from("/test.md"),
        };
        let display = format!("{kind}");
        assert!(display.starts_with("[error]"));
        assert!(display.contains("/test.md"));
    }

    #[test]
    fn serde_roundtrip() {
        let kind = ArchiveAnomalyKind::DuplicateCanonicalId {
            message_id: 99,
            keep_path: PathBuf::from("/keep.md"),
            duplicate_paths: vec![PathBuf::from("/dup1.md"), PathBuf::from("/dup2.md")],
        };
        let anomaly = ArchiveAnomaly::now(kind);
        let json = serde_json::to_string(&anomaly).expect("serialize");
        let roundtripped: ArchiveAnomaly = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(roundtripped.kind, anomaly.kind);
    }

    #[test]
    fn by_remediation_class_filter() {
        let mut report = ArchiveAnomalyReport::new();
        report.record(ArchiveAnomalyKind::MissingProjectMetadata {
            project_dir: PathBuf::from("/a"),
            fallback_slug: "a".to_string(),
        });
        report.record(ArchiveAnomalyKind::OrphanedAgentProfile {
            profile_path: PathBuf::from("/b/profile.json"),
            agent_name: "X".to_string(),
            parent_project_dir: PathBuf::from("/b"),
        });

        assert_eq!(
            report
                .by_remediation_class(RemediationClass::SafeAuto)
                .len(),
            1
        );
        assert_eq!(
            report
                .by_remediation_class(RemediationClass::ManualOnly)
                .len(),
            1
        );
        assert_eq!(
            report
                .by_remediation_class(RemediationClass::ReportOnly)
                .len(),
            0
        );
    }
}
