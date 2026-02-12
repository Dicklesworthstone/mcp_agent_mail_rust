//! Full reindex and DB-vs-index consistency checking
//!
//! Provides two primary workflows:
//! 1. **Full reindex** — drains all documents from a [`DocumentSource`] and rebuilds
//!    the index via [`IndexLifecycle`], writing checkpoints for crash recovery.
//! 2. **Consistency check** — compares DB document counts and version stamps against
//!    the index state to detect drift, orphans, and missing documents.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::document::DocChange;
use crate::engine::{DocumentSource, IndexHealth, IndexLifecycle, IndexStats};
use crate::error::SearchResult;
use crate::index_layout::{IndexCheckpoint, IndexLayout, IndexScope, SchemaHash};

// ── Consistency check types ──────────────────────────────────────────────────

/// Severity level for a consistency finding
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational — no action needed
    Info,
    /// Potential issue — should be investigated
    Warning,
    /// Definite problem — repair recommended
    Error,
}

/// A single consistency finding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyFinding {
    /// Short machine-readable category (e.g. `count_mismatch`, `schema_drift`)
    pub category: String,
    /// Severity of the finding
    pub severity: Severity,
    /// Human-readable description
    pub message: String,
    /// Suggested remediation action (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Result of a consistency check run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyReport {
    /// Individual findings, sorted by severity (errors first)
    pub findings: Vec<ConsistencyFinding>,
    /// Whether the index is considered healthy overall
    pub healthy: bool,
    /// Whether a rebuild is recommended
    pub rebuild_recommended: bool,
    /// Wall-clock time for the check
    pub elapsed_ms: u64,
}

impl ConsistencyReport {
    /// Returns the number of findings at `Error` severity
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }

    /// Returns the number of findings at `Warning` severity
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count()
    }
}

// ── Consistency checker ──────────────────────────────────────────────────────

/// Configuration for consistency checks
#[derive(Debug, Clone)]
pub struct ConsistencyConfig {
    /// Maximum acceptable percentage difference in doc count before flagging
    /// a count mismatch warning (0.0..=1.0)
    pub count_drift_threshold: f64,
}

impl Default for ConsistencyConfig {
    fn default() -> Self {
        Self {
            count_drift_threshold: 0.05, // 5%
        }
    }
}

/// Checks DB-vs-index consistency and produces a report.
///
/// This is intentionally synchronous — async scheduling is done at the wiring layer.
pub fn check_consistency(
    source: &dyn DocumentSource,
    lifecycle: &dyn IndexLifecycle,
    layout: &IndexLayout,
    scope: &IndexScope,
    schema: &SchemaHash,
    config: &ConsistencyConfig,
) -> SearchResult<ConsistencyReport> {
    let start = Instant::now();
    let mut findings = Vec::new();
    let mut rebuild_recommended = false;

    // 1. Check index health
    let health = lifecycle.health();
    check_index_health(&health, &mut findings, &mut rebuild_recommended);

    // 2. Check schema compatibility
    check_schema_compat(
        layout,
        scope,
        schema,
        &mut findings,
        &mut rebuild_recommended,
    );

    // 3. Check checkpoint state
    check_checkpoint(
        layout,
        scope,
        schema,
        &mut findings,
        &mut rebuild_recommended,
    );

    // 4. Compare document counts
    check_doc_counts(
        source,
        &health,
        config,
        &mut findings,
        &mut rebuild_recommended,
    )?;

    // Sort findings: errors first, then warnings, then info
    findings.sort_by_key(|f| match f.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });

    let healthy = !rebuild_recommended && findings.iter().all(|f| f.severity != Severity::Error);

    Ok(ConsistencyReport {
        findings,
        healthy,
        rebuild_recommended,
        elapsed_ms: elapsed_ms_saturating(start),
    })
}

/// Convert elapsed time to u64 milliseconds (saturating at `u64::MAX`)
fn elapsed_ms_saturating(start: Instant) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    let ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    ms
}

fn check_index_health(
    health: &IndexHealth,
    findings: &mut Vec<ConsistencyFinding>,
    rebuild_recommended: &mut bool,
) {
    if health.ready {
        findings.push(ConsistencyFinding {
            category: "index_ready".to_owned(),
            severity: Severity::Info,
            message: format!("Index is ready with {} documents", health.doc_count),
            suggestion: None,
        });
    } else {
        findings.push(ConsistencyFinding {
            category: "index_not_ready".to_owned(),
            severity: Severity::Error,
            message: format!("Index is not ready: {}", health.status_message),
            suggestion: Some("Run a full reindex to rebuild the index.".to_owned()),
        });
        *rebuild_recommended = true;
    }
}

fn check_schema_compat(
    layout: &IndexLayout,
    scope: &IndexScope,
    schema: &SchemaHash,
    findings: &mut Vec<ConsistencyFinding>,
    rebuild_recommended: &mut bool,
) {
    if !layout.is_schema_compatible(scope, "lexical", schema) {
        findings.push(ConsistencyFinding {
            category: "schema_drift".to_owned(),
            severity: Severity::Error,
            message: "Active lexical index schema does not match current schema".to_owned(),
            suggestion: Some("Run a full reindex to rebuild with the current schema.".to_owned()),
        });
        *rebuild_recommended = true;
    }

    if !layout.is_schema_compatible(scope, "semantic", schema) {
        findings.push(ConsistencyFinding {
            category: "schema_drift".to_owned(),
            severity: Severity::Warning,
            message: "Active semantic index schema does not match current schema".to_owned(),
            suggestion: Some(
                "Run a full reindex to rebuild semantic index with the current schema.".to_owned(),
            ),
        });
    }
}

fn check_checkpoint(
    layout: &IndexLayout,
    scope: &IndexScope,
    schema: &SchemaHash,
    findings: &mut Vec<ConsistencyFinding>,
    rebuild_recommended: &mut bool,
) {
    let lexical_dir = layout.lexical_dir(scope, schema);
    match IndexCheckpoint::read_from(&lexical_dir) {
        Ok(cp) => {
            if !cp.success {
                findings.push(ConsistencyFinding {
                    category: "incomplete_build".to_owned(),
                    severity: Severity::Error,
                    message: "Last index build did not complete successfully".to_owned(),
                    suggestion: Some("Run a full reindex to complete the build.".to_owned()),
                });
                *rebuild_recommended = true;
            } else if cp.completed_ts.is_none() {
                findings.push(ConsistencyFinding {
                    category: "incomplete_build".to_owned(),
                    severity: Severity::Warning,
                    message: "Index checkpoint has no completion timestamp".to_owned(),
                    suggestion: Some("Consider running a full reindex.".to_owned()),
                });
            }
        }
        Err(_) => {
            // No checkpoint file — may be first build or file was lost
            findings.push(ConsistencyFinding {
                category: "missing_checkpoint".to_owned(),
                severity: Severity::Warning,
                message: "No checkpoint file found for the current schema version".to_owned(),
                suggestion: Some("Run a full reindex to create a checkpoint.".to_owned()),
            });
        }
    }
}

fn check_doc_counts(
    source: &dyn DocumentSource,
    health: &IndexHealth,
    config: &ConsistencyConfig,
    findings: &mut Vec<ConsistencyFinding>,
    rebuild_recommended: &mut bool,
) -> SearchResult<()> {
    let db_count = source.total_count()?;
    let index_count = health.doc_count;

    if db_count == 0 && index_count == 0 {
        findings.push(ConsistencyFinding {
            category: "empty_index".to_owned(),
            severity: Severity::Info,
            message: "Both DB and index are empty".to_owned(),
            suggestion: None,
        });
        return Ok(());
    }

    if db_count == index_count {
        findings.push(ConsistencyFinding {
            category: "count_match".to_owned(),
            severity: Severity::Info,
            message: format!("Document counts match: {db_count} in both DB and index"),
            suggestion: None,
        });
    } else {
        let abs_diff = db_count.abs_diff(index_count);
        let max_count = db_count.max(index_count);
        // drift as percentage (0..100), using integer math then convert at the end
        let drift_pct = (abs_diff * 100) / max_count;
        // Threshold is 0.0..=1.0, multiply by 100 for percentage comparison
        // This is always a small positive value (0..100), so truncation is safe
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let threshold_pct = (config.count_drift_threshold * 100.0) as usize;

        let severity = if drift_pct > threshold_pct {
            *rebuild_recommended = true;
            Severity::Error
        } else {
            Severity::Warning
        };

        findings.push(ConsistencyFinding {
            category: "count_mismatch".to_owned(),
            severity,
            message: format!(
                "Document count mismatch: DB has {db_count}, index has {index_count} ({drift_pct}% drift)",
            ),
            suggestion: Some(if drift_pct > threshold_pct {
                "Significant drift detected. Run a full reindex.".to_owned()
            } else {
                "Minor drift detected. Incremental updates should resolve this.".to_owned()
            }),
        });
    }

    Ok(())
}

// ── Full reindex ─────────────────────────────────────────────────────────────

/// Configuration for a full reindex operation
#[derive(Debug, Clone)]
pub struct ReindexConfig {
    /// Number of documents to fetch per batch from the source
    pub batch_size: usize,
    /// Whether to write a checkpoint after completion
    pub write_checkpoint: bool,
}

impl Default for ReindexConfig {
    fn default() -> Self {
        Self {
            batch_size: 500,
            write_checkpoint: true,
        }
    }
}

/// Progress callback for reindex operations
pub trait ReindexProgress: Send + Sync {
    /// Called periodically during reindex with current progress
    fn on_progress(&self, indexed: usize, total: usize);
}

/// No-op progress tracker
pub struct NoProgress;

impl ReindexProgress for NoProgress {
    fn on_progress(&self, _indexed: usize, _total: usize) {}
}

/// Result of a full reindex operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReindexResult {
    /// Index statistics from the rebuild
    pub stats: IndexStats,
    /// Whether a checkpoint was written
    pub checkpoint_written: bool,
    /// Total wall-clock time for the reindex
    pub elapsed_ms: u64,
}

/// Perform a full reindex: drain all documents from the source, rebuild the
/// index, and optionally write a checkpoint.
///
/// This uses the batched `DocumentSource` interface to avoid loading the entire
/// corpus into memory, converting each batch into `DocChange::Upsert` operations
/// for the `IndexLifecycle` backend.
///
/// # Errors
/// Returns `SearchError` on data access or index write failures.
pub fn full_reindex(
    source: &dyn DocumentSource,
    lifecycle: &dyn IndexLifecycle,
    layout: &IndexLayout,
    scope: &IndexScope,
    schema: &SchemaHash,
    config: &ReindexConfig,
    progress: &dyn ReindexProgress,
) -> SearchResult<ReindexResult> {
    let start = Instant::now();
    let started_ts = chrono::Utc::now().timestamp_micros();

    // Ensure index directories exist
    layout.ensure_dirs(scope, schema)?;

    // Get total count for progress reporting
    let total = source.total_count()?;
    progress.on_progress(0, total);

    // First, do a full rebuild via the lifecycle (which may clear the index)
    let mut combined_stats = lifecycle.rebuild()?;

    // Then batch-fetch all documents and apply as incremental upserts
    let mut offset = 0;
    let mut total_applied = 0;
    let mut max_version: i64 = 0;

    loop {
        let batch = source.fetch_all_batched(config.batch_size, offset)?;
        if batch.is_empty() {
            break;
        }

        let batch_len = batch.len();

        // Track max version (created_ts) for the checkpoint
        for doc in &batch {
            if doc.created_ts > max_version {
                max_version = doc.created_ts;
            }
        }

        let changes: Vec<DocChange> = batch.into_iter().map(DocChange::Upsert).collect();
        let applied = lifecycle.update_incremental(&changes)?;
        total_applied += applied;

        offset += batch_len;
        progress.on_progress(offset, total);

        // Stop if we got fewer than batch_size (no more data)
        if batch_len < config.batch_size {
            break;
        }
    }

    combined_stats.docs_indexed = total_applied;
    combined_stats.elapsed_ms = elapsed_ms_saturating(start);

    // Write checkpoint
    let mut checkpoint_written = false;
    if config.write_checkpoint {
        let checkpoint = IndexCheckpoint {
            schema_hash: schema.clone(),
            docs_indexed: total_applied,
            started_ts,
            completed_ts: Some(chrono::Utc::now().timestamp_micros()),
            max_version,
            success: true,
        };
        let lexical_dir = layout.lexical_dir(scope, schema);
        match checkpoint.write_to(&lexical_dir) {
            Ok(()) => checkpoint_written = true,
            Err(e) => {
                combined_stats
                    .warnings
                    .push(format!("Failed to write checkpoint: {e}"));
            }
        }
    }

    Ok(ReindexResult {
        stats: combined_stats,
        checkpoint_written,
        elapsed_ms: elapsed_ms_saturating(start),
    })
}

/// Quick repair: runs a consistency check and, if a rebuild is recommended,
/// performs a full reindex.
///
/// Returns the consistency report and optional reindex result.
///
/// # Errors
/// Returns `SearchError` on data access or index write failures.
pub fn repair_if_needed(
    source: &dyn DocumentSource,
    lifecycle: &dyn IndexLifecycle,
    layout: &IndexLayout,
    scope: &IndexScope,
    schema: &SchemaHash,
    progress: &dyn ReindexProgress,
) -> SearchResult<(ConsistencyReport, Option<ReindexResult>)> {
    let report = check_consistency(
        source,
        lifecycle,
        layout,
        scope,
        schema,
        &ConsistencyConfig::default(),
    )?;

    let reindex_result = if report.rebuild_recommended {
        Some(full_reindex(
            source,
            lifecycle,
            layout,
            scope,
            schema,
            &ReindexConfig::default(),
            progress,
        )?)
    } else {
        None
    };

    Ok((report, reindex_result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocKind, Document};
    use crate::engine::IndexHealth;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Mock implementations ──

    struct MockSource {
        docs: Vec<Document>,
    }

    impl MockSource {
        fn new(count: usize) -> Self {
            let docs = (0..count)
                .map(|i| {
                    let idx = i64::try_from(i).expect("count fits i64");
                    Document {
                        id: idx + 1,
                        kind: DocKind::Message,
                        body: format!("body {i}"),
                        title: format!("title {i}"),
                        project_id: Some(1),
                        created_ts: 1_700_000_000_000_000 + idx,
                        metadata: HashMap::new(),
                    }
                })
                .collect();
            Self { docs }
        }

        fn empty() -> Self {
            Self { docs: Vec::new() }
        }
    }

    impl DocumentSource for MockSource {
        fn fetch_batch(&self, ids: &[i64]) -> SearchResult<Vec<Document>> {
            Ok(self
                .docs
                .iter()
                .filter(|d| ids.contains(&d.id))
                .cloned()
                .collect())
        }

        fn fetch_all_batched(
            &self,
            batch_size: usize,
            offset: usize,
        ) -> SearchResult<Vec<Document>> {
            Ok(self
                .docs
                .iter()
                .skip(offset)
                .take(batch_size)
                .cloned()
                .collect())
        }

        fn total_count(&self) -> SearchResult<usize> {
            Ok(self.docs.len())
        }
    }

    struct MockLifecycle {
        doc_count: AtomicUsize,
        ready: bool,
    }

    impl MockLifecycle {
        fn healthy(doc_count: usize) -> Self {
            Self {
                doc_count: AtomicUsize::new(doc_count),
                ready: true,
            }
        }

        fn not_ready() -> Self {
            Self {
                doc_count: AtomicUsize::new(0),
                ready: false,
            }
        }
    }

    impl IndexLifecycle for MockLifecycle {
        fn rebuild(&self) -> SearchResult<IndexStats> {
            self.doc_count.store(0, Ordering::Relaxed);
            Ok(IndexStats {
                docs_indexed: 0,
                docs_removed: 0,
                elapsed_ms: 0,
                warnings: Vec::new(),
            })
        }

        fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize> {
            self.doc_count.fetch_add(changes.len(), Ordering::Relaxed);
            Ok(changes.len())
        }

        fn health(&self) -> IndexHealth {
            IndexHealth {
                ready: self.ready,
                doc_count: self.doc_count.load(Ordering::Relaxed),
                size_bytes: None,
                last_updated_ts: None,
                status_message: if self.ready {
                    "healthy".to_owned()
                } else {
                    "not ready".to_owned()
                },
            }
        }
    }

    struct TrackingProgress {
        calls: Mutex<Vec<(usize, usize)>>,
    }

    impl TrackingProgress {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().expect("lock").len()
        }
    }

    impl ReindexProgress for TrackingProgress {
        fn on_progress(&self, indexed: usize, total: usize) {
            self.calls.lock().expect("lock").push((indexed, total));
        }
    }

    // ── Consistency check tests ──

    #[test]
    fn consistency_healthy_index() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // Set up checkpoint
        layout.ensure_dirs(&scope, &schema).unwrap();
        let cp = IndexCheckpoint {
            schema_hash: schema.clone(),
            docs_indexed: 10,
            started_ts: 1_700_000_000_000_000,
            completed_ts: Some(1_700_000_001_000_000),
            max_version: 1_700_000_000_000_000,
            success: true,
        };
        cp.write_to(&layout.lexical_dir(&scope, &schema)).unwrap();

        let source = MockSource::new(10);
        let lifecycle = MockLifecycle::healthy(10);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(report.healthy);
        assert!(!report.rebuild_recommended);
        assert_eq!(report.error_count(), 0);
    }

    #[test]
    fn consistency_index_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::empty();
        let lifecycle = MockLifecycle::not_ready();

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(!report.healthy);
        assert!(report.rebuild_recommended);
        assert!(report.error_count() > 0);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.category == "index_not_ready")
        );
    }

    #[test]
    fn consistency_count_mismatch_minor() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // DB has 100, index has 99 — 1% drift (under 5% threshold)
        let source = MockSource::new(100);
        let lifecycle = MockLifecycle::healthy(99);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(!report.rebuild_recommended);
        let mismatch = report
            .findings
            .iter()
            .find(|f| f.category == "count_mismatch");
        assert!(mismatch.is_some());
        assert_eq!(mismatch.unwrap().severity, Severity::Warning);
    }

    #[test]
    fn consistency_count_mismatch_severe() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // DB has 100, index has 50 — 50% drift (way over 5% threshold)
        let source = MockSource::new(100);
        let lifecycle = MockLifecycle::healthy(50);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(report.rebuild_recommended);
        let mismatch = report
            .findings
            .iter()
            .find(|f| f.category == "count_mismatch");
        assert!(mismatch.is_some());
        assert_eq!(mismatch.unwrap().severity, Severity::Error);
    }

    #[test]
    fn consistency_missing_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::new(10);
        let lifecycle = MockLifecycle::healthy(10);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.category == "missing_checkpoint")
        );
    }

    #[test]
    fn consistency_incomplete_build() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        layout.ensure_dirs(&scope, &schema).unwrap();
        let cp = IndexCheckpoint {
            schema_hash: schema.clone(),
            docs_indexed: 5,
            started_ts: 1_700_000_000_000_000,
            completed_ts: None,
            max_version: 0,
            success: false,
        };
        cp.write_to(&layout.lexical_dir(&scope, &schema)).unwrap();

        let source = MockSource::new(10);
        let lifecycle = MockLifecycle::healthy(5);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(report.rebuild_recommended);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.category == "incomplete_build")
        );
    }

    #[test]
    fn consistency_both_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::empty();
        let lifecycle = MockLifecycle::healthy(0);

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        assert!(report.healthy);
        assert!(report.findings.iter().any(|f| f.category == "empty_index"));
    }

    #[test]
    fn consistency_findings_sorted_by_severity() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // Not ready + count mismatch = multiple findings at different severities
        let source = MockSource::new(100);
        let lifecycle = MockLifecycle::not_ready();

        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig::default(),
        )
        .unwrap();

        // Verify errors come before warnings come before info
        let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
        for window in severities.windows(2) {
            let ord_a = match window[0] {
                Severity::Error => 0,
                Severity::Warning => 1,
                Severity::Info => 2,
            };
            let ord_b = match window[1] {
                Severity::Error => 0,
                Severity::Warning => 1,
                Severity::Info => 2,
            };
            assert!(ord_a <= ord_b, "Findings should be sorted: errors first");
        }
    }

    // ── Reindex tests ──

    #[test]
    fn full_reindex_empty_source() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::empty();
        let lifecycle = MockLifecycle::healthy(0);
        let progress = NoProgress;

        let result = full_reindex(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ReindexConfig::default(),
            &progress,
        )
        .unwrap();

        assert_eq!(result.stats.docs_indexed, 0);
        assert!(result.checkpoint_written);
    }

    #[test]
    fn full_reindex_with_documents() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Project { project_id: 1 };
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::new(25);
        let lifecycle = MockLifecycle::healthy(0);
        let progress = TrackingProgress::new();

        let result = full_reindex(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ReindexConfig {
                batch_size: 10,
                write_checkpoint: true,
            },
            &progress,
        )
        .unwrap();

        assert_eq!(result.stats.docs_indexed, 25);
        assert!(result.checkpoint_written);
        // Should have progress calls: initial (0,25), after batch1 (10,25),
        // after batch2 (20,25), after batch3 (25,25)
        assert!(progress.call_count() >= 3);

        // Verify checkpoint was written
        let cp = IndexCheckpoint::read_from(&layout.lexical_dir(&scope, &schema)).unwrap();
        assert!(cp.success);
        assert_eq!(cp.docs_indexed, 25);
        assert!(cp.completed_ts.is_some());
    }

    #[test]
    fn full_reindex_no_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::new(5);
        let lifecycle = MockLifecycle::healthy(0);

        let result = full_reindex(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ReindexConfig {
                batch_size: 100,
                write_checkpoint: false,
            },
            &NoProgress,
        )
        .unwrap();

        assert_eq!(result.stats.docs_indexed, 5);
        assert!(!result.checkpoint_written);
    }

    #[test]
    fn full_reindex_resets_index() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // Start with stale data in index
        let source = MockSource::new(10);
        let lifecycle = MockLifecycle::healthy(50);

        let result = full_reindex(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ReindexConfig::default(),
            &NoProgress,
        )
        .unwrap();

        // rebuild() resets to 0, then 10 docs added
        assert_eq!(result.stats.docs_indexed, 10);
        assert_eq!(lifecycle.doc_count.load(Ordering::Relaxed), 10);
    }

    // ── Repair tests ──

    #[test]
    fn repair_skips_reindex_when_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        layout.ensure_dirs(&scope, &schema).unwrap();
        let cp = IndexCheckpoint {
            schema_hash: schema.clone(),
            docs_indexed: 10,
            started_ts: 1_700_000_000_000_000,
            completed_ts: Some(1_700_000_001_000_000),
            max_version: 1_700_000_000_000_000,
            success: true,
        };
        cp.write_to(&layout.lexical_dir(&scope, &schema)).unwrap();

        let source = MockSource::new(10);
        let lifecycle = MockLifecycle::healthy(10);

        let (report, reindex_result) =
            repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress).unwrap();

        assert!(report.healthy);
        assert!(reindex_result.is_none());
    }

    #[test]
    fn repair_triggers_reindex_when_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        let source = MockSource::new(5);
        let lifecycle = MockLifecycle::not_ready();

        let (report, reindex_result) =
            repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress).unwrap();

        assert!(!report.healthy);
        assert!(reindex_result.is_some());
        let result = reindex_result.unwrap();
        assert_eq!(result.stats.docs_indexed, 5);
    }

    #[test]
    fn consistency_report_error_and_warning_counts() {
        let report = ConsistencyReport {
            findings: vec![
                ConsistencyFinding {
                    category: "a".to_owned(),
                    severity: Severity::Error,
                    message: "err1".to_owned(),
                    suggestion: None,
                },
                ConsistencyFinding {
                    category: "b".to_owned(),
                    severity: Severity::Error,
                    message: "err2".to_owned(),
                    suggestion: None,
                },
                ConsistencyFinding {
                    category: "c".to_owned(),
                    severity: Severity::Warning,
                    message: "warn1".to_owned(),
                    suggestion: None,
                },
                ConsistencyFinding {
                    category: "d".to_owned(),
                    severity: Severity::Info,
                    message: "info1".to_owned(),
                    suggestion: None,
                },
            ],
            healthy: false,
            rebuild_recommended: true,
            elapsed_ms: 42,
        };

        assert_eq!(report.error_count(), 2);
        assert_eq!(report.warning_count(), 1);
    }

    #[test]
    fn severity_serde_roundtrip() {
        for sev in &[Severity::Info, Severity::Warning, Severity::Error] {
            let json = serde_json::to_string(sev).unwrap();
            let back: Severity = serde_json::from_str(&json).unwrap();
            assert_eq!(*sev, back);
        }
    }

    #[test]
    fn finding_serde_with_suggestion() {
        let finding = ConsistencyFinding {
            category: "test".to_owned(),
            severity: Severity::Warning,
            message: "something".to_owned(),
            suggestion: Some("fix it".to_owned()),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: ConsistencyFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back.suggestion.as_deref(), Some("fix it"));
    }

    #[test]
    fn finding_serde_without_suggestion() {
        let finding = ConsistencyFinding {
            category: "test".to_owned(),
            severity: Severity::Info,
            message: "ok".to_owned(),
            suggestion: None,
        };
        let json = serde_json::to_string(&finding).unwrap();
        assert!(!json.contains("suggestion"));
    }

    #[test]
    fn reindex_result_serde() {
        let result = ReindexResult {
            stats: IndexStats {
                docs_indexed: 100,
                docs_removed: 5,
                elapsed_ms: 500,
                warnings: vec!["warn".to_owned()],
            },
            checkpoint_written: true,
            elapsed_ms: 600,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ReindexResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stats.docs_indexed, 100);
        assert!(back.checkpoint_written);
    }

    #[test]
    fn custom_count_drift_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = IndexLayout::new(tmp.path());
        let scope = IndexScope::Global;
        let schema = SchemaHash("abc123456789".to_owned());

        // DB has 100, index has 90 — 10% drift
        let source = MockSource::new(100);
        let lifecycle = MockLifecycle::healthy(90);

        // With 5% threshold, 10% drift → error + rebuild
        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig {
                count_drift_threshold: 0.05,
            },
        )
        .unwrap();
        assert!(report.rebuild_recommended);

        // With 15% threshold, 10% drift → warning only
        let report = check_consistency(
            &source,
            &lifecycle,
            &layout,
            &scope,
            &schema,
            &ConsistencyConfig {
                count_drift_threshold: 0.15,
            },
        )
        .unwrap();
        assert!(!report.rebuild_recommended);
    }
}
