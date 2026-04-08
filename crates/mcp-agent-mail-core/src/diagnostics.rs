//! Structured diagnostic report combining all system health metrics.
//!
//! Provides a comprehensive snapshot for operators debugging issues with
//! 1000+ concurrent agents. Includes system info, database, storage,
//! tools, lock contention, health level, and automated recommendations.
//!
//! # Usage
//!
//! ```rust,ignore
//! let report = DiagnosticReport::build(tool_snapshot, slow_tools);
//! let json = serde_json::to_string_pretty(&report).unwrap();
//! ```

#![forbid(unsafe_code)]

use serde::Serialize;

use crate::backpressure::{self, HealthLevel, HealthSignals};
use crate::lock_order::{LockContentionEntry, lock_contention_snapshot};
use crate::metrics::{
    DbMetricsSnapshot, GlobalMetricsSnapshot, HttpMetricsSnapshot, SearchMetricsSnapshot,
    StorageMetricsSnapshot, SystemMetricsSnapshot, ToolsMetricsSnapshot, global_metrics,
};

/// Maximum serialized report size in bytes (100KB).
const MAX_REPORT_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// Top-level diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    /// Report generation timestamp (ISO-8601).
    pub generated_at: String,
    /// System information (uptime, Rust version, OS, CPU count).
    pub system: SystemInfo,
    /// Health level assessment.
    pub health: HealthInfo,
    /// HTTP request metrics.
    pub http: HttpMetricsSnapshot,
    /// Aggregate tool call metrics.
    pub tools_aggregate: ToolsMetricsSnapshot,
    /// Per-tool call/error/latency snapshots (passed in from tools crate).
    pub tools_detail: Vec<serde_json::Value>,
    /// Slow tools (p95 > 500ms), passed in from tools crate.
    pub slow_tools: Vec<serde_json::Value>,
    /// Database pool metrics.
    pub database: DbMetricsSnapshot,
    /// Storage (WBQ + commit queue) metrics.
    pub storage: StorageMetricsSnapshot,
    /// Search V3 metrics (query volume, fallback, shadow, index health).
    pub search: SearchMetricsSnapshot,
    /// Disk usage metrics.
    pub disk: SystemMetricsSnapshot,
    /// Lock contention metrics.
    pub locks: Vec<LockContentionEntry>,
    /// Automated recommendations based on current metrics.
    pub recommendations: Vec<Recommendation>,
}

/// System information gathered at report time.
#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    /// Process uptime in seconds.
    pub uptime_secs: u64,
    /// Rust compiler version used to build.
    pub rust_version: &'static str,
    /// Target architecture.
    pub target: &'static str,
    /// Operating system description.
    pub os: String,
    /// Number of available CPUs.
    pub cpu_count: usize,
}

/// Health level with underlying signal breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct HealthInfo {
    /// Current health level: `"green"`, `"yellow"`, or `"red"`.
    pub level: String,
    /// Underlying signals that drive the health classification.
    pub signals: HealthSignals,
}

/// A single recommendation for the operator.
#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    /// Severity: `"info"`, `"warning"`, `"critical"`.
    pub severity: &'static str,
    /// Which subsystem the recommendation relates to.
    pub subsystem: &'static str,
    /// Human-readable recommendation text.
    pub message: String,
}

/// Severity bucket for archive scan findings after operator-facing condensation.
///
/// This is intentionally narrower than the raw anomaly taxonomy. The goal is to
/// separate stop-what-you-are-doing issues from hygiene debt while keeping
/// terminal output compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveScanSeverityBucket {
    /// Low-risk oddities that belong in artifacts rather than loud terminal output.
    Info,
    /// Actionable hygiene debt that should be scheduled and remediated.
    Warning,
    /// Immediate operator attention is required before trusting recovery or promotion.
    Critical,
}

impl ArchiveScanSeverityBucket {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Info => 1,
            Self::Warning => 2,
            Self::Critical => 3,
        }
    }
}

impl std::fmt::Display for ArchiveScanSeverityBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a deduped archive finding blocks progress now or is hygiene debt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveScanScope {
    /// This finding should be treated as action-now operator work.
    ImmediateAction,
    /// This finding is real but should not eclipse live incident handling.
    HygieneDebt,
}

impl ArchiveScanScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImmediateAction => "immediate_action",
            Self::HygieneDebt => "hygiene_debt",
        }
    }

    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::ImmediateAction => 2,
            Self::HygieneDebt => 1,
        }
    }
}

impl std::fmt::Display for ArchiveScanScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable dedupe rule for archive scan findings.
///
/// This makes it explicit why repeated low-level findings collapse into a
/// single operator-facing summary row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveScanDedupeRule {
    /// One logical archive message id may fan out to many duplicate file paths.
    MessageId,
    /// Project metadata drift should collapse to one row per project directory.
    ProjectDir,
    /// Corrupt or malformed canonical content is distinct per file path.
    CanonicalPath,
    /// Profile corruption is distinct per agent profile path.
    AgentProfilePath,
    /// Identity drift is distinct per archive identity pair.
    ArchiveIdentity,
    /// No richer scope exists; collapse by finding kind only.
    KindOnly,
}

impl ArchiveScanDedupeRule {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MessageId => "message_id",
            Self::ProjectDir => "project_dir",
            Self::CanonicalPath => "canonical_path",
            Self::AgentProfilePath => "agent_profile_path",
            Self::ArchiveIdentity => "archive_identity",
            Self::KindOnly => "kind_only",
        }
    }
}

impl std::fmt::Display for ArchiveScanDedupeRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One machine-readable archive scan diagnostic before summary condensation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveScanDiagnostic {
    /// Stable anomaly or finding code, for example `duplicate_canonical_id`.
    pub code: String,
    /// Operator-facing severity bucket.
    pub severity: ArchiveScanSeverityBucket,
    /// Whether this finding is urgent now or hygiene debt.
    pub scope: ArchiveScanScope,
    /// How callers should dedupe repeated low-level findings.
    pub dedupe_rule: ArchiveScanDedupeRule,
    /// Scope value used with `dedupe_rule` to construct the stable dedupe key.
    pub dedupe_value: String,
    /// Concise one-line summary for the deduped group.
    pub summary: String,
    /// Short next-step guidance when the finding survives summary condensation.
    pub recommendation: Option<String>,
}

impl ArchiveScanDiagnostic {
    /// Stable dedupe key used by `ArchiveScanSummary`.
    #[must_use]
    pub fn dedupe_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.code,
            self.dedupe_rule.as_str(),
            self.dedupe_value
        )
    }
}

/// Deduped finding retained in the concise summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveScanSummaryFinding {
    /// Stable finding code.
    pub code: String,
    /// Whether this is immediate-action work or hygiene debt.
    pub scope: ArchiveScanScope,
    /// Dedupe rule used to collapse repeated low-level findings.
    pub dedupe_rule: ArchiveScanDedupeRule,
    /// Stable summary key (`code:rule:value`).
    pub dedupe_key: String,
    /// Number of raw findings collapsed into this summary row.
    pub occurrence_count: usize,
    /// Concise operator copy for the deduped group.
    pub summary: String,
    /// Optional next-step guidance for this group.
    pub recommendation: Option<String>,
}

/// Per-severity bucket in the concise archive scan summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveScanSummaryBucket {
    /// Severity bucket represented by this section.
    pub severity: ArchiveScanSeverityBucket,
    /// Raw finding count before dedupe.
    pub raw_count: usize,
    /// Unique finding groups after dedupe.
    pub deduped_count: usize,
    /// Number of additional deduped groups omitted from `findings`.
    pub overflow_count: usize,
    /// Representative findings for terminal-friendly output.
    pub findings: Vec<ArchiveScanSummaryFinding>,
}

/// Compact, operator-oriented summary for archive scan diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveScanSummary {
    /// Highest severity present, or `None` when the archive is clean.
    pub highest_severity: Option<ArchiveScanSeverityBucket>,
    /// One-line summary optimized for doctor/startup surfaces.
    pub headline: String,
    /// Best next action at the summary level.
    pub next_action: Option<String>,
    /// Raw finding count before dedupe.
    pub total_findings: usize,
    /// Unique finding groups after dedupe.
    pub deduped_findings: usize,
    /// Deduped groups that require operator attention now.
    pub immediate_action_count: usize,
    /// Deduped groups that are hygiene debt.
    pub hygiene_debt_count: usize,
    /// Maximum number of representative findings retained per bucket.
    pub sample_limit: usize,
    /// Severity-ordered summary buckets.
    pub buckets: Vec<ArchiveScanSummaryBucket>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveScanSummaryAccumulator {
    code: String,
    severity: ArchiveScanSeverityBucket,
    scope: ArchiveScanScope,
    dedupe_rule: ArchiveScanDedupeRule,
    dedupe_key: String,
    summary: String,
    recommendation: Option<String>,
    occurrence_count: usize,
}

impl ArchiveScanSummaryAccumulator {
    fn from_diagnostic(diagnostic: ArchiveScanDiagnostic) -> Self {
        let dedupe_key = diagnostic.dedupe_key();
        Self {
            code: diagnostic.code,
            severity: diagnostic.severity,
            scope: diagnostic.scope,
            dedupe_rule: diagnostic.dedupe_rule,
            dedupe_key,
            summary: diagnostic.summary,
            recommendation: diagnostic.recommendation,
            occurrence_count: 1,
        }
    }

    fn merge(&mut self, diagnostic: ArchiveScanDiagnostic) {
        self.occurrence_count = self.occurrence_count.saturating_add(1);
        let candidate_severity = diagnostic.severity.priority();
        let existing_severity = self.severity.priority();
        let candidate_scope = diagnostic.scope.priority();
        let existing_scope = self.scope.priority();
        if candidate_severity > existing_severity
            || (candidate_severity == existing_severity && candidate_scope > existing_scope)
        {
            self.severity = diagnostic.severity;
            self.scope = diagnostic.scope;
            self.summary = diagnostic.summary;
            self.recommendation = diagnostic.recommendation;
        } else if self.recommendation.is_none() {
            self.recommendation = diagnostic.recommendation;
        }
    }
}

impl ArchiveScanSummary {
    fn empty(sample_limit: usize) -> Self {
        Self {
            highest_severity: None,
            headline: "No archive scan findings detected.".to_string(),
            next_action: None,
            total_findings: 0,
            deduped_findings: 0,
            immediate_action_count: 0,
            hygiene_debt_count: 0,
            sample_limit,
            buckets: Vec::new(),
        }
    }

    fn accumulate<I>(
        diagnostics: I,
    ) -> (
        std::collections::BTreeMap<ArchiveScanSeverityBucket, usize>,
        std::collections::BTreeMap<String, ArchiveScanSummaryAccumulator>,
    )
    where
        I: IntoIterator<Item = ArchiveScanDiagnostic>,
    {
        let mut raw_counts: std::collections::BTreeMap<ArchiveScanSeverityBucket, usize> =
            std::collections::BTreeMap::new();
        let mut deduped: std::collections::BTreeMap<String, ArchiveScanSummaryAccumulator> =
            std::collections::BTreeMap::new();
        for diagnostic in diagnostics {
            raw_counts
                .entry(diagnostic.severity)
                .and_modify(|count: &mut usize| *count = count.saturating_add(1))
                .or_insert(1);
            let key = diagnostic.dedupe_key();
            match deduped.entry(key) {
                std::collections::btree_map::Entry::Occupied(mut existing) => {
                    existing.get_mut().merge(diagnostic);
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(ArchiveScanSummaryAccumulator::from_diagnostic(diagnostic));
                }
            }
        }
        (raw_counts, deduped)
    }

    fn headline_and_next_action(
        raw_total: usize,
        immediate_action_count: usize,
        hygiene_debt_count: usize,
    ) -> (String, Option<String>) {
        if immediate_action_count > 0 {
            (
                format!(
                    "Immediate action required: {immediate_action_count} group(s) need operator review now; {hygiene_debt_count} group(s) are hygiene debt.",
                ),
                Some(
                    "Inspect immediate-action groups before trusting archive recovery or promotion; keep terminal output concise and use full artifacts for per-path detail."
                        .to_string(),
                ),
            )
        } else {
            (
                format!(
                    "Archive hygiene debt detected: {hygiene_debt_count} deduped group(s) across {raw_total} raw finding(s).",
                ),
                Some(
                    "Use the concise summary for triage, then inspect full artifact detail only for the remaining hygiene-debt groups."
                        .to_string(),
                ),
            )
        }
    }

    fn summarize_buckets(
        raw_counts: &std::collections::BTreeMap<ArchiveScanSeverityBucket, usize>,
        deduped: std::collections::BTreeMap<String, ArchiveScanSummaryAccumulator>,
        sample_limit: usize,
    ) -> Vec<ArchiveScanSummaryBucket> {
        let mut bucket_findings: std::collections::BTreeMap<
            ArchiveScanSeverityBucket,
            Vec<ArchiveScanSummaryFinding>,
        > = std::collections::BTreeMap::new();
        for item in deduped.into_values() {
            bucket_findings
                .entry(item.severity)
                .or_default()
                .push(ArchiveScanSummaryFinding {
                    code: item.code,
                    scope: item.scope,
                    dedupe_rule: item.dedupe_rule,
                    dedupe_key: item.dedupe_key,
                    occurrence_count: item.occurrence_count,
                    summary: item.summary,
                    recommendation: item.recommendation,
                });
        }

        let mut buckets = Vec::new();
        for severity in [
            ArchiveScanSeverityBucket::Critical,
            ArchiveScanSeverityBucket::Warning,
            ArchiveScanSeverityBucket::Info,
        ] {
            let Some(mut findings) = bucket_findings.remove(&severity) else {
                continue;
            };
            findings.sort_by(|left, right| {
                right
                    .scope
                    .priority()
                    .cmp(&left.scope.priority())
                    .then_with(|| right.occurrence_count.cmp(&left.occurrence_count))
                    .then_with(|| left.code.cmp(&right.code))
                    .then_with(|| left.dedupe_key.cmp(&right.dedupe_key))
            });
            let deduped_count = findings.len();
            let limited_findings: Vec<_> = findings.into_iter().take(sample_limit).collect();
            buckets.push(ArchiveScanSummaryBucket {
                severity,
                raw_count: raw_counts.get(&severity).copied().unwrap_or(0),
                deduped_count,
                overflow_count: deduped_count.saturating_sub(limited_findings.len()),
                findings: limited_findings,
            });
        }

        buckets
    }

    /// Build a concise, severity-bucketed summary from raw archive findings.
    #[must_use]
    pub fn build<I>(diagnostics: I, sample_limit: usize) -> Self
    where
        I: IntoIterator<Item = ArchiveScanDiagnostic>,
    {
        let diagnostics: Vec<_> = diagnostics.into_iter().collect();
        if diagnostics.is_empty() {
            return Self::empty(sample_limit);
        }

        let (raw_counts, deduped) = Self::accumulate(diagnostics);
        let raw_total = raw_counts.values().copied().sum();
        let deduped_findings = deduped.len();
        let highest_severity = deduped
            .values()
            .map(|item| item.severity)
            .max_by_key(|sev| sev.priority());
        let immediate_action_count = deduped
            .values()
            .filter(|item| item.scope == ArchiveScanScope::ImmediateAction)
            .count();
        let hygiene_debt_count = deduped_findings.saturating_sub(immediate_action_count);
        let (headline, next_action) =
            Self::headline_and_next_action(raw_total, immediate_action_count, hygiene_debt_count);
        let buckets = Self::summarize_buckets(&raw_counts, deduped, sample_limit);

        Self {
            highest_severity,
            headline,
            next_action,
            total_findings: raw_total,
            deduped_findings,
            immediate_action_count,
            hygiene_debt_count,
            sample_limit,
            buckets,
        }
    }
}

// ---------------------------------------------------------------------------
// Machine-readable diagnostic payloads with artifact pointers
// ---------------------------------------------------------------------------

/// Status of an artifact referenced from a diagnostic payload.
///
/// Vocabulary matches the forensic bundle manifest spec (v1):
/// `captured`, `missing`, `referenced`, or `skipped`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Artifact file was captured and exists on disk at the referenced path.
    Captured,
    /// Artifact was expected but not found (e.g. WAL/SHM file absent).
    Missing,
    /// Artifact content is not copied into the payload; the path points to
    /// the canonical on-disk location where it can be read separately.
    Referenced,
    /// Artifact capture was intentionally skipped (e.g. dry-run mode).
    Skipped,
}

impl ArtifactStatus {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::Missing => "missing",
            Self::Referenced => "referenced",
            Self::Skipped => "skipped",
        }
    }
}

/// A pointer to a single artifact file relevant to a diagnostic surface.
///
/// Downstream consumers use these to locate forensic bundles, scan reports,
/// database files, and archive directories without parsing wall-of-text output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactPointer {
    /// Machine-stable artifact kind, e.g. `"forensic_bundle"`, `"scan_report"`,
    /// `"sqlite_db"`, `"archive_root"`, `"wal_sidecar"`.
    pub kind: String,
    /// Absolute path to the artifact on disk. May be absent for referenced-only
    /// artifacts that were not materialized.
    pub path: Option<String>,
    /// Capture status per the forensic bundle manifest vocabulary.
    pub status: ArtifactStatus,
    /// Human-readable label for the artifact in diagnostic display contexts.
    pub label: String,
    /// Optional additional detail (e.g. integrity status, byte count, schema).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ArtifactPointer {
    /// Convenience constructor for a captured artifact at a known path.
    #[must_use]
    pub fn captured(kind: &str, path: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            path: Some(path.to_string()),
            status: ArtifactStatus::Captured,
            label: label.to_string(),
            detail: None,
        }
    }

    /// Convenience constructor for a referenced (not copied) artifact.
    #[must_use]
    pub fn referenced(kind: &str, path: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            path: Some(path.to_string()),
            status: ArtifactStatus::Referenced,
            label: label.to_string(),
            detail: None,
        }
    }

    /// Convenience constructor for a missing artifact.
    #[must_use]
    pub fn missing(kind: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            path: None,
            status: ArtifactStatus::Missing,
            label: label.to_string(),
            detail: None,
        }
    }

    /// Convenience constructor for a skipped artifact.
    #[must_use]
    pub fn skipped(kind: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            path: None,
            status: ArtifactStatus::Skipped,
            label: label.to_string(),
            detail: None,
        }
    }

    /// Set optional detail text and return self for builder chaining.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Machine-readable diagnostic payload emitted by doctor and archive-scan
/// commands, designed for programmatic consumption by other surfaces.
///
/// This struct avoids duplicating full scan or check detail inline. Instead it
/// carries a concise summary alongside [`ArtifactPointer`] entries that let
/// consumers locate full detail artifacts on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticPayload {
    /// Schema identity for forward-compatible parsing.
    pub schema: DiagnosticPayloadSchema,
    /// Which command or surface produced this payload.
    pub source: String,
    /// ISO-8601 timestamp when the payload was generated.
    pub generated_at: String,
    /// Overall status: `"ok"`, `"warn"`, or `"fail"`.
    pub status: String,
    /// One-line headline for the diagnostic result.
    pub headline: String,
    /// Optional next-action guidance for the operator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    /// Machine-readable finding counts by severity.
    pub finding_counts: DiagnosticFindingCounts,
    /// Pointers to artifacts that hold full diagnostic detail.
    pub artifacts: Vec<ArtifactPointer>,
}

/// Schema version for [`DiagnosticPayload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticPayloadSchema {
    /// Schema name.
    pub name: &'static str,
    /// Major version (breaking changes).
    pub major: u32,
    /// Minor version (additive-only changes).
    pub minor: u32,
}

/// Per-severity finding counts embedded in the diagnostic payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct DiagnosticFindingCounts {
    /// Number of critical-severity findings.
    pub critical: usize,
    /// Number of warning-severity findings.
    pub warning: usize,
    /// Number of info-severity findings.
    pub info: usize,
    /// Total raw finding count before any deduplication.
    pub total: usize,
}

impl DiagnosticPayload {
    /// Schema name for all diagnostic payloads.
    pub const SCHEMA_NAME: &'static str = "mcp-agent-mail-diagnostic-payload";
    /// Current schema major version.
    pub const SCHEMA_MAJOR: u32 = 1;
    /// Current schema minor version.
    pub const SCHEMA_MINOR: u32 = 0;

    /// Build a diagnostic payload from a doctor check result.
    #[must_use]
    pub fn from_doctor_check(
        status: &str,
        headline: &str,
        fail_count: usize,
        warn_count: usize,
        artifacts: Vec<ArtifactPointer>,
    ) -> Self {
        Self {
            schema: DiagnosticPayloadSchema {
                name: Self::SCHEMA_NAME,
                major: Self::SCHEMA_MAJOR,
                minor: Self::SCHEMA_MINOR,
            },
            source: "doctor-check".to_string(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
            headline: headline.to_string(),
            next_action: if status == "ok" {
                None
            } else {
                Some("Run `am doctor fix --dry-run` to preview remediation.".to_string())
            },
            finding_counts: DiagnosticFindingCounts {
                critical: fail_count,
                warning: warn_count,
                info: 0,
                total: fail_count.saturating_add(warn_count),
            },
            artifacts,
        }
    }

    /// Build a diagnostic payload from an archive scan summary.
    #[must_use]
    pub fn from_archive_scan(
        summary: &ArchiveScanSummary,
        artifacts: Vec<ArtifactPointer>,
    ) -> Self {
        let status = match summary.highest_severity {
            Some(ArchiveScanSeverityBucket::Critical) => "fail",
            Some(ArchiveScanSeverityBucket::Warning) => "warn",
            Some(ArchiveScanSeverityBucket::Info) | None => "ok",
        };
        let mut critical = 0usize;
        let mut warning = 0usize;
        let mut info = 0usize;
        for bucket in &summary.buckets {
            match bucket.severity {
                ArchiveScanSeverityBucket::Critical => {
                    critical = critical.saturating_add(bucket.raw_count);
                }
                ArchiveScanSeverityBucket::Warning => {
                    warning = warning.saturating_add(bucket.raw_count);
                }
                ArchiveScanSeverityBucket::Info => {
                    info = info.saturating_add(bucket.raw_count);
                }
            }
        }
        Self {
            schema: DiagnosticPayloadSchema {
                name: Self::SCHEMA_NAME,
                major: Self::SCHEMA_MAJOR,
                minor: Self::SCHEMA_MINOR,
            },
            source: "archive-scan".to_string(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
            headline: summary.headline.clone(),
            next_action: summary.next_action.clone(),
            finding_counts: DiagnosticFindingCounts {
                critical,
                warning,
                info,
                total: summary.total_findings,
            },
            artifacts,
        }
    }
}

// ---------------------------------------------------------------------------
// Warning flood gate — cap terminal noise while preserving full detail
// ---------------------------------------------------------------------------

/// Default per-category cap for terminal-visible warnings.
pub const DEFAULT_WARNING_CAP_PER_CATEGORY: usize = 3;

/// A single warning entry captured by the flood gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CappedWarning {
    /// Stable category key for dedupe (e.g. `"reconstruct_parse_error"`,
    /// `"duplicate_canonical_message"`, `"sanitized_thread_id"`).
    pub category: String,
    /// Full warning message (always preserved in artifacts).
    pub message: String,
}

/// Per-category overflow statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarningCategoryOverflow {
    /// Stable category key.
    pub category: String,
    /// Number of warnings that were captured in this category.
    pub total: usize,
    /// Number that fit within the cap (shown to the operator).
    pub shown: usize,
    /// Number suppressed from terminal output.
    pub suppressed: usize,
}

/// Summary of warnings after flood-gate processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarningFloodSummary {
    /// Total warnings across all categories.
    pub total_warnings: usize,
    /// Total warnings shown to the operator.
    pub total_shown: usize,
    /// Total warnings suppressed.
    pub total_suppressed: usize,
    /// Number of distinct categories with at least one warning.
    pub category_count: usize,
    /// Number of categories that had overflow.
    pub overflow_category_count: usize,
    /// Per-category overflow detail.
    pub overflows: Vec<WarningCategoryOverflow>,
    /// One-line summary sentence for terminal output. `None` if nothing was suppressed.
    pub suppression_notice: Option<String>,
}

/// Caps terminal warning floods while preserving the complete warning list
/// for machine-readable artifact output.
///
/// # Usage
///
/// ```rust,ignore
/// let mut gate = WarningFloodGate::new(3);
/// for warning in warnings {
///     gate.push("parse_error", &warning);
/// }
/// // Terminal output: gate.terminal_warnings() (capped)
/// // Artifact detail: gate.all_warnings() (complete)
/// // Summary: gate.summary()
/// ```
#[derive(Debug, Clone)]
pub struct WarningFloodGate {
    cap_per_category: usize,
    /// All warnings in insertion order (never capped).
    all_warnings: Vec<CappedWarning>,
    /// Per-category count of warnings seen.
    category_counts: std::collections::BTreeMap<String, usize>,
}

impl WarningFloodGate {
    /// Create a new flood gate with the given per-category cap.
    #[must_use]
    pub fn new(cap_per_category: usize) -> Self {
        Self {
            cap_per_category: cap_per_category.max(1),
            all_warnings: Vec::new(),
            category_counts: std::collections::BTreeMap::new(),
        }
    }

    /// Create a flood gate with the default cap.
    #[must_use]
    pub fn default_cap() -> Self {
        Self::new(DEFAULT_WARNING_CAP_PER_CATEGORY)
    }

    /// Record a warning. Always preserved in the full list; only shown on
    /// terminal if the per-category cap hasn't been reached.
    ///
    /// Returns `true` if this warning is within the cap (should be shown),
    /// `false` if it was suppressed from terminal output.
    pub fn push(&mut self, category: &str, message: impl Into<String>) -> bool {
        let message = message.into();
        self.all_warnings.push(CappedWarning {
            category: category.to_string(),
            message,
        });
        let count = self
            .category_counts
            .entry(category.to_string())
            .or_insert(0);
        *count = count.saturating_add(1);
        *count <= self.cap_per_category
    }

    /// Record a warning from an existing `String`, returning the within-cap status.
    pub fn push_owned(&mut self, category: String, message: String) -> bool {
        self.all_warnings.push(CappedWarning {
            category: category.clone(),
            message,
        });
        let count = self.category_counts.entry(category).or_insert(0);
        *count = count.saturating_add(1);
        *count <= self.cap_per_category
    }

    /// Total number of warnings across all categories.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.all_warnings.len()
    }

    /// Whether any warnings were suppressed from terminal output.
    #[must_use]
    pub fn has_suppressed(&self) -> bool {
        self.category_counts
            .values()
            .any(|&count| count > self.cap_per_category)
    }

    /// Complete list of all warnings (for artifact/JSON output).
    #[must_use]
    pub fn all_warnings(&self) -> &[CappedWarning] {
        &self.all_warnings
    }

    /// Warnings that fit within the per-category cap (for terminal output).
    #[must_use]
    pub fn terminal_warnings(&self) -> Vec<&CappedWarning> {
        let mut per_category_seen: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        self.all_warnings
            .iter()
            .filter(|warning| {
                let seen = per_category_seen.entry(&warning.category).or_insert(0);
                *seen += 1;
                *seen <= self.cap_per_category
            })
            .collect()
    }

    /// Extract just the terminal-visible warning messages (convenience).
    #[must_use]
    pub fn terminal_messages(&self) -> Vec<&str> {
        self.terminal_warnings()
            .into_iter()
            .map(|w| w.message.as_str())
            .collect()
    }

    /// Extract all warning messages (convenience for artifact output).
    #[must_use]
    pub fn all_messages(&self) -> Vec<&str> {
        self.all_warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect()
    }

    /// Build a summary of flood-gate state.
    #[must_use]
    pub fn summary(&self) -> WarningFloodSummary {
        let total_warnings = self.all_warnings.len();
        let mut total_shown = 0usize;
        let mut overflows = Vec::new();
        for (category, &count) in &self.category_counts {
            let shown = count.min(self.cap_per_category);
            let suppressed = count.saturating_sub(self.cap_per_category);
            total_shown = total_shown.saturating_add(shown);
            if suppressed > 0 {
                overflows.push(WarningCategoryOverflow {
                    category: category.clone(),
                    total: count,
                    shown,
                    suppressed,
                });
            }
        }
        let total_suppressed = total_warnings.saturating_sub(total_shown);
        let overflow_category_count = overflows.len();

        let suppression_notice = if total_suppressed > 0 {
            let category_detail: Vec<String> = overflows
                .iter()
                .map(|o| format!("{}: {} suppressed", o.category, o.suppressed))
                .collect();
            Some(format!(
                "{total_suppressed} warning(s) suppressed from terminal output \
                 across {overflow_category_count} category/categories; \
                 full detail in artifacts. [{detail}]",
                detail = category_detail.join("; ")
            ))
        } else {
            None
        };

        WarningFloodSummary {
            total_warnings,
            total_shown,
            total_suppressed,
            category_count: self.category_counts.len(),
            overflow_category_count,
            overflows,
            suppression_notice,
        }
    }

    /// Consume the gate and return all warnings as owned strings (for
    /// compatibility with existing `Vec<String>` warning fields).
    #[must_use]
    pub fn into_all_messages(self) -> Vec<String> {
        self.all_warnings.into_iter().map(|w| w.message).collect()
    }

    /// Consume the gate and return only terminal-visible warnings as owned
    /// strings.
    #[must_use]
    pub fn into_terminal_messages(self) -> Vec<String> {
        let cap = self.cap_per_category;
        let mut per_category_seen: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        self.all_warnings
            .into_iter()
            .filter(|warning| {
                let seen = per_category_seen
                    .entry(warning.category.clone())
                    .or_insert(0);
                *seen += 1;
                *seen <= cap
            })
            .map(|w| w.message)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Static system info
// ---------------------------------------------------------------------------

/// Process start time for uptime calculation.
static PROCESS_START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// Call early in `main()` to anchor uptime measurement.
pub fn init_process_start() {
    let _ = &*PROCESS_START;
}

#[inline]
pub fn process_uptime() -> std::time::Duration {
    PROCESS_START.elapsed()
}

fn system_info() -> SystemInfo {
    let uptime = process_uptime();
    SystemInfo {
        uptime_secs: uptime.as_secs(),
        rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("nightly"),
        target: std::env::consts::ARCH,
        os: std::env::consts::OS.to_string(),
        cpu_count: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
    }
}

// ---------------------------------------------------------------------------
// Recommendation engine
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)] // deliberate: metric values fit in f64
fn health_recommendations(
    health: HealthLevel,
    signals: &HealthSignals,
    recs: &mut Vec<Recommendation>,
) {
    match health {
        HealthLevel::Red => recs.push(Recommendation {
            severity: "critical",
            subsystem: "health",
            message: "System is in RED health state. Shedding low-priority tool calls. \
                      Investigate pool utilization, WBQ depth, and commit queue."
                .into(),
        }),
        HealthLevel::Yellow => recs.push(Recommendation {
            severity: "warning",
            subsystem: "health",
            message: "System is in YELLOW health state. Load is elevated but not critical.".into(),
        }),
        HealthLevel::Green => {}
    }

    // Pool utilization
    if signals.pool_utilization_pct >= 90 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "database",
            message: format!(
                "Pool utilization at {}%. Consider increasing DATABASE_POOL_SIZE.",
                signals.pool_utilization_pct,
            ),
        });
    } else if signals.pool_utilization_pct >= 70 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "database",
            message: format!(
                "Pool utilization at {}%. Monitor for growth.",
                signals.pool_utilization_pct,
            ),
        });
    }

    // Pool acquire latency
    if signals.pool_acquire_p95_us > 100_000 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "database",
            message: format!(
                "Pool acquire p95 latency is {:.1}ms. Consider increasing pool size or \
                 reducing concurrent tool calls.",
                signals.pool_acquire_p95_us as f64 / 1000.0,
            ),
        });
    }

    // WBQ depth
    if signals.wbq_depth_pct >= 80 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "storage",
            message: format!(
                "Write-back queue at {}% capacity. Archive writes may be backing up.",
                signals.wbq_depth_pct,
            ),
        });
    }

    // Commit queue
    if signals.commit_depth_pct >= 80 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "storage",
            message: format!(
                "Commit queue at {}% capacity. Git commits may be falling behind.",
                signals.commit_depth_pct,
            ),
        });
    }
}

#[allow(clippy::cast_precision_loss)] // deliberate: metric values fit in f64
fn operational_recommendations(
    snap: &GlobalMetricsSnapshot,
    lock_snap: &[LockContentionEntry],
    slow_tool_count: usize,
    recs: &mut Vec<Recommendation>,
) {
    // Slow tools
    if slow_tool_count > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "tools",
            message: format!(
                "{slow_tool_count} tool(s) have p95 latency > 500ms. Check tools_detail for specifics.",
            ),
        });
    }

    // High error rate
    let tool_calls = snap.tools.tool_calls_total;
    let tool_errors = snap.tools.tool_errors_total;
    if tool_calls > 100 {
        let error_pct = (tool_errors as f64 / tool_calls as f64) * 100.0;
        if error_pct > 10.0 {
            recs.push(Recommendation {
                severity: "warning",
                subsystem: "tools",
                message: format!(
                    "Tool error rate is {error_pct:.1}% ({tool_errors}/{tool_calls}). Investigate failing tools.",
                ),
            });
        }
    }

    // Lock contention
    for entry in lock_snap {
        if entry.contention_ratio > 0.1 && entry.acquire_count > 100 {
            recs.push(Recommendation {
                severity: "warning",
                subsystem: "locks",
                message: format!(
                    "Lock '{}' has {:.1}% contention rate ({} contended / {} acquires). \
                     Max wait: {:.2}ms.",
                    entry.lock_name,
                    entry.contention_ratio * 100.0,
                    entry.contended_count,
                    entry.acquire_count,
                    entry.max_wait_ns as f64 / 1_000_000.0,
                ),
            });
        }
    }

    // Disk pressure
    if snap.system.disk_pressure_level >= 2 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "disk",
            message: format!(
                "Disk pressure level {} \u{2014} storage free: {} bytes, DB free: {} bytes.",
                snap.system.disk_pressure_level,
                snap.system.disk_storage_free_bytes,
                snap.system.disk_db_free_bytes,
            ),
        });
    }

    // Search rollout health
    let search = &snap.search;
    if search.fallback_to_legacy_total > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Search V3 fallback-to-legacy count is {}. Investigate Tantivy/V3 availability.",
                search.fallback_to_legacy_total
            ),
        });
    }
    if search.shadow_v3_errors_total > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Shadow mode observed {} V3 errors. Review Search V3 logs before widening rollout.",
                search.shadow_v3_errors_total
            ),
        });
    }
    if search.shadow_comparisons_total >= 10 && search.shadow_equivalent_pct < 80.0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Shadow equivalence is {:.1}% over {} comparisons; below 80% parity target.",
                search.shadow_equivalent_pct, search.shadow_comparisons_total
            ),
        });
    }
    if search.queries_v3_total > 0 && search.tantivy_doc_count == 0 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "search",
            message: "V3 queries are executing but Tantivy doc_count is 0. Validate index build and ingest.".to_string(),
        });
    }
}

fn generate_recommendations(
    snap: &GlobalMetricsSnapshot,
    health: HealthLevel,
    signals: &HealthSignals,
    lock_snap: &[LockContentionEntry],
    slow_tool_count: usize,
) -> Vec<Recommendation> {
    let mut recs = Vec::with_capacity(8);
    health_recommendations(health, signals, &mut recs);
    operational_recommendations(snap, lock_snap, slow_tool_count, &mut recs);
    recs
}

// ---------------------------------------------------------------------------
// Report builder
// ---------------------------------------------------------------------------

impl DiagnosticReport {
    /// Build a comprehensive diagnostic report.
    ///
    /// `tools_detail` and `slow_tools` are passed in as `serde_json::Value`
    /// because the per-tool `MetricsSnapshotEntry` type lives in the tools
    /// crate (which depends on core, not the other way around). The server or
    /// tools crate serializes these before passing them in.
    #[must_use]
    pub fn build(tools_detail: Vec<serde_json::Value>, slow_tools: Vec<serde_json::Value>) -> Self {
        let snap = global_metrics().snapshot();
        let (health_level, signals) = backpressure::compute_health_level_with_signals();
        let lock_snap = lock_contention_snapshot();
        let slow_tool_count = slow_tools.len();

        let recs =
            generate_recommendations(&snap, health_level, &signals, &lock_snap, slow_tool_count);

        Self {
            generated_at: chrono::Utc::now().to_rfc3339(),
            system: system_info(),
            health: HealthInfo {
                level: health_level.as_str().to_string(),
                signals,
            },
            http: snap.http,
            tools_aggregate: snap.tools,
            tools_detail,
            slow_tools,
            database: snap.db,
            storage: snap.storage,
            search: snap.search,
            disk: snap.system,
            locks: lock_snap,
            recommendations: recs,
        }
    }

    /// Serialize to JSON, truncating if the report exceeds 100KB.
    #[must_use]
    pub fn to_json(&self) -> String {
        match serde_json::to_string_pretty(self) {
            Ok(json) if json.len() <= MAX_REPORT_BYTES => json,
            Ok(json) => {
                // Truncate tools_detail to fit within budget.
                let mut truncated = self.clone();
                let mut tools_truncated = false;
                let mut locks_truncated = false;
                while serde_json::to_string(&truncated).map_or(0, |s| s.len()) > MAX_REPORT_BYTES {
                    if !tools_truncated && truncated.tools_detail.len() > 5 {
                        truncated.tools_detail.truncate(5);
                        truncated.tools_detail.push(serde_json::json!({
                            "_truncated": true,
                            "_message": "tools_detail truncated to fit 100KB report limit"
                        }));
                        tools_truncated = true;
                    } else if !locks_truncated && truncated.locks.len() > 5 {
                        truncated.locks.truncate(5);
                        locks_truncated = true;
                    } else {
                        // Give up. Return a valid JSON error object instead of broken JSON.
                        return serde_json::json!({
                            "error": "report too large",
                            "message": "diagnostic report exceeded 100KB limit even after truncation",
                            "size_bytes": json.len()
                        })
                        .to_string();
                    }
                }
                serde_json::to_string_pretty(&truncated).unwrap_or(json)
            }
            Err(_) => r#"{"error":"failed to serialize diagnostic report"}"#.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_process_start_is_idempotent() {
        init_process_start();
        let before = process_uptime();

        std::thread::sleep(std::time::Duration::from_millis(25));
        init_process_start();
        let after = process_uptime();

        assert!(
            after >= before + std::time::Duration::from_millis(10),
            "process uptime appears to have been reset: before={before:?} after={after:?}"
        );
    }

    #[test]
    fn report_builds_without_panic() {
        let report = DiagnosticReport::build(vec![], vec![]);
        assert!(!report.generated_at.is_empty());
        assert!(report.system.cpu_count >= 1);
        assert_eq!(report.health.level, "green");
    }

    #[test]
    fn report_json_serializable() {
        let report = DiagnosticReport::build(vec![], vec![]);
        let json = report.to_json();
        assert!(!json.is_empty());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.get("generated_at").is_some());
        assert!(parsed.get("health").is_some());
        assert!(parsed.get("search").is_some());
        assert!(parsed.get("recommendations").is_some());
    }

    #[test]
    fn report_respects_size_limit() {
        // Build a report with lots of tool detail to test truncation.
        let big_tools: Vec<serde_json::Value> = (0..1000)
            .map(|i| {
                serde_json::json!({
                    "name": format!("tool_{i}"),
                    "calls": i,
                    "errors": 0,
                    "cluster": "test",
                    "padding": "x".repeat(200),
                })
            })
            .collect();
        let report = DiagnosticReport::build(big_tools, vec![]);
        let json = report.to_json();
        assert!(
            json.len() <= MAX_REPORT_BYTES + 1024, // small grace for truncation boundary
            "report too large: {} bytes",
            json.len()
        );
    }

    #[test]
    fn recommendations_for_healthy_system() {
        let report = DiagnosticReport::build(vec![], vec![]);
        assert_eq!(report.health.level, "green");
        assert!(
            !report
                .recommendations
                .iter()
                .any(|r| r.severity == "critical"),
            "healthy system should have no critical recommendations"
        );
    }

    #[test]
    fn slow_tools_generates_recommendation() {
        let slow = vec![serde_json::json!({
            "name": "send_message",
            "p95_ms": 600.0,
        })];
        let report = DiagnosticReport::build(vec![], slow);
        assert!(
            report
                .recommendations
                .iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("p95")),
            "should warn about slow tools"
        );
    }

    #[test]
    fn system_info_populated() {
        let info = system_info();
        assert!(info.cpu_count >= 1);
        assert!(!info.os.is_empty());
    }

    fn archive_scan_diagnostic(
        code: &str,
        severity: ArchiveScanSeverityBucket,
        scope: ArchiveScanScope,
        dedupe_rule: ArchiveScanDedupeRule,
        dedupe_value: &str,
        summary: &str,
    ) -> ArchiveScanDiagnostic {
        ArchiveScanDiagnostic {
            code: code.to_string(),
            severity,
            scope,
            dedupe_rule,
            dedupe_value: dedupe_value.to_string(),
            summary: summary.to_string(),
            recommendation: None,
        }
    }

    #[test]
    fn archive_scan_summary_empty_is_clean() {
        let summary = ArchiveScanSummary::build(Vec::<ArchiveScanDiagnostic>::new(), 3);
        assert_eq!(summary.highest_severity, None);
        assert_eq!(summary.total_findings, 0);
        assert_eq!(summary.deduped_findings, 0);
        assert!(summary.buckets.is_empty());
        assert_eq!(summary.headline, "No archive scan findings detected.");
        assert!(summary.next_action.is_none());
    }

    #[test]
    fn archive_scan_summary_dedupes_and_counts_scope() {
        let summary = ArchiveScanSummary::build(
            vec![
                archive_scan_diagnostic(
                    "missing_project_metadata",
                    ArchiveScanSeverityBucket::Warning,
                    ArchiveScanScope::HygieneDebt,
                    ArchiveScanDedupeRule::ProjectDir,
                    "/archive/projects/demo",
                    "missing project.json for demo",
                ),
                archive_scan_diagnostic(
                    "missing_project_metadata",
                    ArchiveScanSeverityBucket::Warning,
                    ArchiveScanScope::HygieneDebt,
                    ArchiveScanDedupeRule::ProjectDir,
                    "/archive/projects/demo",
                    "missing project.json for demo",
                ),
                archive_scan_diagnostic(
                    "malformed_message",
                    ArchiveScanSeverityBucket::Critical,
                    ArchiveScanScope::ImmediateAction,
                    ArchiveScanDedupeRule::CanonicalPath,
                    "/archive/projects/demo/messages/2026/04/bad.md",
                    "canonical message file is malformed",
                ),
            ],
            3,
        );

        assert_eq!(
            summary.highest_severity,
            Some(ArchiveScanSeverityBucket::Critical)
        );
        assert_eq!(summary.total_findings, 3);
        assert_eq!(summary.deduped_findings, 2);
        assert_eq!(summary.immediate_action_count, 1);
        assert_eq!(summary.hygiene_debt_count, 1);
        assert_eq!(summary.buckets.len(), 2);
        assert_eq!(
            summary.buckets[0].severity,
            ArchiveScanSeverityBucket::Critical
        );
        assert_eq!(summary.buckets[0].raw_count, 1);
        assert_eq!(summary.buckets[0].deduped_count, 1);
        assert_eq!(
            summary.buckets[1].severity,
            ArchiveScanSeverityBucket::Warning
        );
        assert_eq!(summary.buckets[1].raw_count, 2);
        assert_eq!(summary.buckets[1].deduped_count, 1);
        assert_eq!(summary.buckets[1].findings[0].occurrence_count, 2);
        assert_eq!(
            summary.buckets[1].findings[0].dedupe_rule,
            ArchiveScanDedupeRule::ProjectDir
        );
    }

    #[test]
    fn archive_scan_summary_respects_sample_limit() {
        let summary = ArchiveScanSummary::build(
            vec![
                archive_scan_diagnostic(
                    "duplicate_canonical_id",
                    ArchiveScanSeverityBucket::Critical,
                    ArchiveScanScope::ImmediateAction,
                    ArchiveScanDedupeRule::MessageId,
                    "7",
                    "message id 7 has duplicate canonical files",
                ),
                archive_scan_diagnostic(
                    "duplicate_canonical_id",
                    ArchiveScanSeverityBucket::Critical,
                    ArchiveScanScope::ImmediateAction,
                    ArchiveScanDedupeRule::MessageId,
                    "9",
                    "message id 9 has duplicate canonical files",
                ),
                archive_scan_diagnostic(
                    "invalid_project_metadata",
                    ArchiveScanSeverityBucket::Critical,
                    ArchiveScanScope::ImmediateAction,
                    ArchiveScanDedupeRule::ProjectDir,
                    "/archive/projects/demo",
                    "project metadata cannot be auto-normalized",
                ),
                archive_scan_diagnostic(
                    "suspicious_ephemeral_project",
                    ArchiveScanSeverityBucket::Info,
                    ArchiveScanScope::HygieneDebt,
                    ArchiveScanDedupeRule::ProjectDir,
                    "/archive/projects/tmp-demo",
                    "project looks ephemeral in the archive",
                ),
            ],
            2,
        );

        assert_eq!(
            summary.buckets[0].severity,
            ArchiveScanSeverityBucket::Critical
        );
        assert_eq!(summary.buckets[0].raw_count, 3);
        assert_eq!(summary.buckets[0].deduped_count, 3);
        assert_eq!(summary.buckets[0].findings.len(), 2);
        assert_eq!(summary.buckets[0].overflow_count, 1);
        assert_eq!(summary.buckets[1].severity, ArchiveScanSeverityBucket::Info);
        assert_eq!(summary.buckets[1].overflow_count, 0);
    }

    // -- health_recommendations direct tests --

    fn zero_signals() -> HealthSignals {
        HealthSignals {
            pool_acquire_p95_us: 0,
            pool_utilization_pct: 0,
            pool_over_80_for_s: 0,
            wbq_depth_pct: 0,
            wbq_over_80_for_s: 0,
            commit_depth_pct: 0,
            commit_over_80_for_s: 0,
        }
    }

    #[test]
    fn health_rec_red_emits_critical() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Red, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "health"),
            "RED health should produce a critical health recommendation"
        );
    }

    #[test]
    fn health_rec_yellow_emits_warning() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Yellow, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "warning" && r.subsystem == "health"),
            "YELLOW health should produce a warning"
        );
    }

    #[test]
    fn health_rec_green_no_health_rec() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "health"),
            "GREEN health should not produce a health recommendation"
        );
    }

    #[test]
    fn health_rec_pool_90_pct_critical() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 95;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "database"),
            "95% pool utilization should trigger critical database recommendation"
        );
    }

    #[test]
    fn health_rec_pool_75_pct_warning() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 75;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "warning" && r.subsystem == "database"),
            "75% pool utilization should trigger warning"
        );
    }

    #[test]
    fn health_rec_pool_50_pct_no_rec() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 50;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "database"),
            "50% pool utilization should not trigger any database recommendation"
        );
    }

    #[test]
    fn health_rec_high_acquire_latency() {
        let mut signals = zero_signals();
        signals.pool_acquire_p95_us = 150_000; // 150ms
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "database" && r.message.contains("latency")),
            "high acquire latency should trigger recommendation"
        );
    }

    #[test]
    fn health_rec_wbq_depth_80() {
        let mut signals = zero_signals();
        signals.wbq_depth_pct = 85;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "storage" && r.message.contains("Write-back")),
            "high WBQ depth should trigger storage recommendation"
        );
    }

    #[test]
    fn health_rec_commit_depth_80() {
        let mut signals = zero_signals();
        signals.commit_depth_pct = 90;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "storage" && r.message.contains("Commit queue")),
            "high commit depth should trigger storage recommendation"
        );
    }

    // -- operational_recommendations direct tests --

    #[test]
    fn ops_rec_slow_tools() {
        let snap = GlobalMetricsSnapshot::default();
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 3, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("3 tool(s)")),
            "should warn about slow tools"
        );
    }

    #[test]
    fn ops_rec_high_error_rate() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 50; // 25%
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("error rate")),
            "25% error rate should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_error_rate_no_warning() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 5; // 2.5%
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.message.contains("error rate")),
            "2.5% error rate should not trigger warning"
        );
    }

    #[test]
    fn ops_rec_few_calls_skips_error_rate() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 10;
        snap.tools.tool_errors_total = 5; // 50% but only 10 calls
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.message.contains("error rate")),
            "should not warn about error rate with < 100 calls"
        );
    }

    #[test]
    fn ops_rec_lock_contention() {
        let snap = GlobalMetricsSnapshot::default();
        let locks = vec![LockContentionEntry {
            lock_name: "TestLock".to_string(),
            rank: 1,
            acquire_count: 500,
            contended_count: 100,
            total_wait_ns: 5_000_000,
            total_hold_ns: 50_000_000,
            max_wait_ns: 1_000_000,
            max_hold_ns: 2_000_000,
            contention_ratio: 0.2, // 20%
        }];
        let mut recs = Vec::new();
        operational_recommendations(&snap, &locks, 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "locks" && r.message.contains("TestLock")),
            "20% contention with 500 acquires should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_contention_no_warning() {
        let snap = GlobalMetricsSnapshot::default();
        let locks = vec![LockContentionEntry {
            lock_name: "TestLock".to_string(),
            rank: 1,
            acquire_count: 500,
            contended_count: 10,
            total_wait_ns: 100_000,
            total_hold_ns: 5_000_000,
            max_wait_ns: 50_000,
            max_hold_ns: 200_000,
            contention_ratio: 0.02, // 2%
        }];
        let mut recs = Vec::new();
        operational_recommendations(&snap, &locks, 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "locks"),
            "2% contention should not trigger warning"
        );
    }

    #[test]
    fn ops_rec_disk_pressure() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.system.disk_pressure_level = 2;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "disk"),
            "disk pressure level 2 should trigger critical recommendation"
        );
    }

    #[test]
    fn ops_rec_search_fallback() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.fallback_to_legacy_total = 5;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("fallback")),
            "search fallback-to-legacy should trigger warning"
        );
    }

    #[test]
    fn ops_rec_shadow_errors() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.shadow_v3_errors_total = 3;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("Shadow mode")),
            "shadow V3 errors should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_shadow_equivalence() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.shadow_comparisons_total = 20;
        snap.search.shadow_equivalent_pct = 60.0;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("equivalence")),
            "60% equivalence with 20+ comparisons should trigger warning"
        );
    }

    #[test]
    fn ops_rec_v3_queries_no_docs() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.queries_v3_total = 10;
        snap.search.tantivy_doc_count = 0;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "search"),
            "V3 queries with empty index should trigger critical warning"
        );
    }

    // -- generate_recommendations aggregation --

    #[test]
    fn generate_recs_combines_health_and_ops() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 50;
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 95;
        let recs = generate_recommendations(&snap, HealthLevel::Red, &signals, &[], 2);
        // Should have health (red), database (pool 95%), tools (slow + error rate)
        assert!(
            recs.len() >= 3,
            "expected at least 3 recommendations, got {}",
            recs.len()
        );
        assert!(recs.iter().any(|r| r.subsystem == "health"));
        assert!(recs.iter().any(|r| r.subsystem == "database"));
        assert!(recs.iter().any(|r| r.subsystem == "tools"));
    }

    // -- ArtifactPointer tests --

    #[test]
    fn artifact_pointer_captured_has_path_and_status() {
        let ap = ArtifactPointer::captured("sqlite_db", "/tmp/test.db", "Test database");
        assert_eq!(ap.kind, "sqlite_db");
        assert_eq!(ap.path.as_deref(), Some("/tmp/test.db"));
        assert_eq!(ap.status, ArtifactStatus::Captured);
        assert_eq!(ap.label, "Test database");
        assert!(ap.detail.is_none());
    }

    #[test]
    fn artifact_pointer_referenced_has_path() {
        let ap = ArtifactPointer::referenced("archive_root", "/tmp/archive", "Archive root");
        assert_eq!(ap.status, ArtifactStatus::Referenced);
        assert!(ap.path.is_some());
    }

    #[test]
    fn artifact_pointer_missing_has_no_path() {
        let ap = ArtifactPointer::missing("wal_sidecar", "WAL file");
        assert_eq!(ap.status, ArtifactStatus::Missing);
        assert!(ap.path.is_none());
    }

    #[test]
    fn artifact_pointer_skipped_has_no_path() {
        let ap = ArtifactPointer::skipped("forensic_bundle", "Forensic bundle");
        assert_eq!(ap.status, ArtifactStatus::Skipped);
        assert!(ap.path.is_none());
    }

    #[test]
    fn artifact_pointer_with_detail_chains() {
        let ap =
            ArtifactPointer::captured("sqlite_db", "/tmp/test.db", "DB").with_detail("bytes=1024");
        assert_eq!(ap.detail.as_deref(), Some("bytes=1024"));
    }

    #[test]
    fn artifact_status_as_str_round_trips() {
        assert_eq!(ArtifactStatus::Captured.as_str(), "captured");
        assert_eq!(ArtifactStatus::Missing.as_str(), "missing");
        assert_eq!(ArtifactStatus::Referenced.as_str(), "referenced");
        assert_eq!(ArtifactStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn artifact_pointer_serializes_to_json() {
        let ap =
            ArtifactPointer::captured("sqlite_db", "/tmp/test.db", "DB").with_detail("bytes=512");
        let json = serde_json::to_value(&ap).expect("serialize");
        assert_eq!(json["kind"], "sqlite_db");
        assert_eq!(json["path"], "/tmp/test.db");
        assert_eq!(json["status"], "captured");
        assert_eq!(json["label"], "DB");
        assert_eq!(json["detail"], "bytes=512");
    }

    #[test]
    fn artifact_pointer_omits_detail_when_none() {
        let ap = ArtifactPointer::missing("wal_sidecar", "WAL file");
        let json = serde_json::to_value(&ap).expect("serialize");
        assert!(
            json.get("detail").is_none(),
            "detail should be omitted via skip_serializing_if"
        );
    }

    // -- DiagnosticPayload tests --

    #[test]
    fn diagnostic_payload_from_doctor_check_ok() {
        let payload = DiagnosticPayload::from_doctor_check(
            "ok",
            "All checks passed.",
            0,
            0,
            vec![ArtifactPointer::referenced("sqlite_db", "/tmp/db", "DB")],
        );
        assert_eq!(payload.schema.name, DiagnosticPayload::SCHEMA_NAME);
        assert_eq!(payload.schema.major, 1);
        assert_eq!(payload.schema.minor, 0);
        assert_eq!(payload.source, "doctor-check");
        assert_eq!(payload.status, "ok");
        assert_eq!(payload.headline, "All checks passed.");
        assert!(payload.next_action.is_none());
        assert_eq!(payload.finding_counts.total, 0);
        assert_eq!(payload.artifacts.len(), 1);
    }

    #[test]
    fn diagnostic_payload_from_doctor_check_fail_has_next_action() {
        let payload =
            DiagnosticPayload::from_doctor_check("fail", "2 check(s) failed.", 2, 1, Vec::new());
        assert_eq!(payload.status, "fail");
        assert!(payload.next_action.is_some());
        assert_eq!(payload.finding_counts.critical, 2);
        assert_eq!(payload.finding_counts.warning, 1);
        assert_eq!(payload.finding_counts.total, 3);
    }

    #[test]
    fn diagnostic_payload_from_archive_scan_empty() {
        let summary = ArchiveScanSummary::build(Vec::<ArchiveScanDiagnostic>::new(), 3);
        let payload = DiagnosticPayload::from_archive_scan(&summary, Vec::new());
        assert_eq!(payload.source, "archive-scan");
        assert_eq!(payload.status, "ok");
        assert_eq!(payload.finding_counts.total, 0);
        assert_eq!(payload.finding_counts.critical, 0);
    }

    #[test]
    fn diagnostic_payload_from_archive_scan_with_findings() {
        let summary = ArchiveScanSummary::build(
            vec![
                archive_scan_diagnostic(
                    "malformed_message",
                    ArchiveScanSeverityBucket::Critical,
                    ArchiveScanScope::ImmediateAction,
                    ArchiveScanDedupeRule::CanonicalPath,
                    "/archive/bad.md",
                    "malformed",
                ),
                archive_scan_diagnostic(
                    "missing_project_metadata",
                    ArchiveScanSeverityBucket::Warning,
                    ArchiveScanScope::HygieneDebt,
                    ArchiveScanDedupeRule::ProjectDir,
                    "/archive/demo",
                    "missing metadata",
                ),
            ],
            3,
        );
        let artifacts = vec![
            ArtifactPointer::referenced("archive_root", "/tmp/archive", "Archive"),
            ArtifactPointer::referenced("sqlite_db", "/tmp/db", "DB"),
        ];
        let payload = DiagnosticPayload::from_archive_scan(&summary, artifacts);
        assert_eq!(payload.status, "fail");
        assert_eq!(payload.finding_counts.critical, 1);
        assert_eq!(payload.finding_counts.warning, 1);
        assert_eq!(payload.finding_counts.total, 2);
        assert_eq!(payload.artifacts.len(), 2);
        assert!(payload.next_action.is_some());
    }

    #[test]
    fn diagnostic_payload_serializes_to_json() {
        let payload = DiagnosticPayload::from_doctor_check(
            "warn",
            "1 warning found.",
            0,
            1,
            vec![
                ArtifactPointer::referenced("sqlite_db", "/tmp/db", "DB"),
                ArtifactPointer::missing("wal_sidecar", "WAL"),
            ],
        );
        let json = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(json["schema"]["name"], DiagnosticPayload::SCHEMA_NAME);
        assert_eq!(json["schema"]["major"], 1);
        assert_eq!(json["source"], "doctor-check");
        assert_eq!(json["status"], "warn");
        assert!(json["artifacts"].is_array());
        assert_eq!(json["artifacts"].as_array().unwrap().len(), 2);
        assert_eq!(json["finding_counts"]["warning"], 1);
        assert_eq!(json["finding_counts"]["total"], 1);
    }

    #[test]
    fn diagnostic_payload_schema_version_constants() {
        assert_eq!(
            DiagnosticPayload::SCHEMA_NAME,
            "mcp-agent-mail-diagnostic-payload"
        );
        assert_eq!(DiagnosticPayload::SCHEMA_MAJOR, 1);
        assert_eq!(DiagnosticPayload::SCHEMA_MINOR, 0);
    }

    // -- WarningFloodGate tests --

    #[test]
    fn flood_gate_empty_has_no_warnings() {
        let gate = WarningFloodGate::default_cap();
        assert_eq!(gate.total(), 0);
        assert!(!gate.has_suppressed());
        assert!(gate.terminal_warnings().is_empty());
        assert!(gate.all_warnings().is_empty());
        let summary = gate.summary();
        assert_eq!(summary.total_warnings, 0);
        assert_eq!(summary.total_suppressed, 0);
        assert!(summary.suppression_notice.is_none());
    }

    #[test]
    fn flood_gate_within_cap_shows_all() {
        let mut gate = WarningFloodGate::new(3);
        assert!(gate.push("parse_error", "file A failed"));
        assert!(gate.push("parse_error", "file B failed"));
        assert!(gate.push("parse_error", "file C failed"));
        assert_eq!(gate.total(), 3);
        assert!(!gate.has_suppressed());
        assert_eq!(gate.terminal_warnings().len(), 3);
        assert_eq!(gate.all_warnings().len(), 3);
    }

    #[test]
    fn flood_gate_exceeds_cap_suppresses() {
        let mut gate = WarningFloodGate::new(2);
        assert!(gate.push("dup", "dup warning 1"));
        assert!(gate.push("dup", "dup warning 2"));
        assert!(!gate.push("dup", "dup warning 3")); // suppressed
        assert!(!gate.push("dup", "dup warning 4")); // suppressed
        assert_eq!(gate.total(), 4);
        assert!(gate.has_suppressed());
        assert_eq!(gate.terminal_warnings().len(), 2);
        assert_eq!(gate.all_warnings().len(), 4);
    }

    #[test]
    fn flood_gate_multiple_categories_independent_caps() {
        let mut gate = WarningFloodGate::new(2);
        gate.push("cat_a", "a1");
        gate.push("cat_a", "a2");
        gate.push("cat_a", "a3"); // suppressed
        gate.push("cat_b", "b1");
        gate.push("cat_b", "b2");
        assert_eq!(gate.total(), 5);
        assert_eq!(gate.terminal_warnings().len(), 4); // 2 from a, 2 from b
        assert_eq!(gate.all_warnings().len(), 5);
    }

    #[test]
    fn flood_gate_summary_reports_overflow() {
        let mut gate = WarningFloodGate::new(2);
        for i in 0..10 {
            gate.push("flood", format!("warning {i}"));
        }
        gate.push("clean", "single warning");

        let summary = gate.summary();
        assert_eq!(summary.total_warnings, 11);
        assert_eq!(summary.total_shown, 3); // 2 flood + 1 clean
        assert_eq!(summary.total_suppressed, 8);
        assert_eq!(summary.category_count, 2);
        assert_eq!(summary.overflow_category_count, 1);
        assert_eq!(summary.overflows.len(), 1);
        assert_eq!(summary.overflows[0].category, "flood");
        assert_eq!(summary.overflows[0].total, 10);
        assert_eq!(summary.overflows[0].shown, 2);
        assert_eq!(summary.overflows[0].suppressed, 8);
        assert!(summary.suppression_notice.is_some());
        let notice = summary.suppression_notice.unwrap();
        assert!(notice.contains("8 warning(s) suppressed"));
        assert!(notice.contains("flood: 8 suppressed"));
    }

    #[test]
    fn flood_gate_no_suppression_notice_when_within_cap() {
        let mut gate = WarningFloodGate::new(5);
        gate.push("a", "w1");
        gate.push("b", "w2");
        let summary = gate.summary();
        assert!(summary.suppression_notice.is_none());
    }

    #[test]
    fn flood_gate_terminal_messages_convenience() {
        let mut gate = WarningFloodGate::new(1);
        gate.push("x", "first");
        gate.push("x", "second");
        let msgs = gate.terminal_messages();
        assert_eq!(msgs, vec!["first"]);
        let all = gate.all_messages();
        assert_eq!(all, vec!["first", "second"]);
    }

    #[test]
    fn flood_gate_into_terminal_messages() {
        let mut gate = WarningFloodGate::new(1);
        gate.push("x", "keep");
        gate.push("x", "drop");
        gate.push("y", "keep_y");
        let msgs = gate.into_terminal_messages();
        assert_eq!(msgs, vec!["keep", "keep_y"]);
    }

    #[test]
    fn flood_gate_into_all_messages() {
        let mut gate = WarningFloodGate::new(1);
        gate.push("x", "a");
        gate.push("x", "b");
        let msgs = gate.into_all_messages();
        assert_eq!(msgs, vec!["a", "b"]);
    }

    #[test]
    fn flood_gate_push_owned() {
        let mut gate = WarningFloodGate::new(1);
        assert!(gate.push_owned("cat".to_string(), "msg1".to_string()));
        assert!(!gate.push_owned("cat".to_string(), "msg2".to_string()));
        assert_eq!(gate.total(), 2);
        assert_eq!(gate.terminal_warnings().len(), 1);
    }

    #[test]
    fn flood_gate_summary_serializes() {
        let mut gate = WarningFloodGate::new(2);
        for i in 0..5 {
            gate.push("test", format!("w{i}"));
        }
        let summary = gate.summary();
        let json = serde_json::to_value(&summary).expect("serialize");
        assert_eq!(json["total_warnings"], 5);
        assert_eq!(json["total_suppressed"], 3);
        assert!(json["suppression_notice"].is_string());
    }

    #[test]
    fn flood_gate_cap_at_least_one() {
        let gate = WarningFloodGate::new(0); // should be clamped to 1
        assert_eq!(gate.cap_per_category, 1);
    }
}
