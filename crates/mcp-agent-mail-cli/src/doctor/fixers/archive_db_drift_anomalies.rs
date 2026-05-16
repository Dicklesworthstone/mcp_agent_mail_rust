//! `fm-archive-state-files-archive-db-drift-anomalies` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! The mailbox archive (filesystem) and SQLite index are two
//! independent stores held in sync by the storage layer's commit
//! coalescer + cache-write-through. When they drift, the doctor
//! cannot tell which side is authoritative without operator input:
//!
//! - **ArchiveDbProjectMismatch** — the archive contains a project
//!   identity (`<storage_root>/projects/<slug>/`) that does NOT
//!   match any row in the `projects` table, OR the archive-side
//!   `human_key` disagrees with the DB-side `path`. Causes:
//!   project deleted from DB but archive not garbage-collected,
//!   archive restored from a backup older than the DB, or two
//!   storage roots being mixed.
//! - **ArchiveDbCountDrift** — the unique message-id count on
//!   disk differs significantly from `SELECT count(*) FROM
//!   messages`. Causes: half-applied migration, partial archive
//!   restore, or DB rebuild that didn't replay every commit.
//!
//! These two variants are bundled into ONE FM because the
//! remediation playbook is identical: inspect the divergence,
//! decide which side is the source of truth, then use the supported
//! recovery path for that direction. `am doctor reconstruct` rebuilds
//! SQLite from the archive. There is intentionally no broad
//! archive-from-DB rewrite command; when the DB is authoritative, preserve
//! the DB and restore or manually rebuild the affected archive artifacts.
//!
//! ## Detection
//!
//! Filters an `ArchiveAnomalyReport` for
//! `ArchiveAnomalyKind::ArchiveDbProjectMismatch` and
//! `ArchiveAnomalyKind::ArchiveDbCountDrift`. In the normal doctor
//! dispatcher this report is produced by
//! `scan_archive_anomalies_with_db(storage_root, db_path)` so the DB-aware
//! variants can actually be observed. Direct detector calls with only a
//! storage root fall back to archive-only scanning and therefore won't emit
//! this FM.
//!
//! ## Fix
//!
//! **Detect-only.** Reconciling DB-vs-archive drift requires
//! operator judgment (which side is authoritative?). The doctor
//! deliberately refuses to pick a side because the wrong call
//! silently destroys data. Manual remediation:
//!
//! 1. Run `am doctor archive-scan --json` to dump the full archive
//!    state.
//! 2. Decide which side is authoritative.
//! 3. If archive is authoritative: `am doctor reconstruct --dry-run --json`
//!    to preview, then `am doctor reconstruct --yes` after preserving
//!    forensics/backups.
//! 4. If DB is authoritative: preserve the DB, restore the archive from a
//!    known-good backup, or manually rebuild the affected archive artifacts.
//!    `am doctor archive-normalize --dry-run` only handles safe archive
//!    hygiene such as project metadata and duplicate canonical files.
//! 5. Re-run this detector to confirm zero residual drift.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-archive-db-drift-anomalies";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMismatchEntry {
    pub archive_slug: String,
    pub archive_human_key: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CountDriftEntry {
    pub archive_count: usize,
    pub db_count: usize,
    pub drift: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveDbDriftFinding {
    pub project_mismatches: Vec<ProjectMismatchEntry>,
    pub count_drifts: Vec<CountDriftEntry>,
}

impl ArchiveDbDriftFinding {
    pub fn total_entries(&self) -> usize {
        self.project_mismatches.len() + self.count_drifts.len()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} archive-vs-DB drift signal(s): {} project identity mismatch(es), {} message-count drift(s)",
            self.total_entries(),
            self.project_mismatches.len(),
            self.count_drifts.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "project_mismatches": self.project_mismatches,
                "count_drifts": self.count_drifts,
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor archive-scan --json` to dump full archive state.",
                        "Decide which side is authoritative: is the DB the source of truth, or the on-disk archive?",
                        "If archive is authoritative: run `am doctor reconstruct --dry-run --json` to preview the DB rebuild, then `am doctor reconstruct --yes` after preserving forensics/backups.",
                        "If DB is authoritative: preserve the DB and restore the archive from a known-good backup or manually rebuild the affected archive artifacts; there is no broad archive-from-DB rewrite command.",
                        "`am doctor archive-normalize --dry-run` is only for safe archive hygiene such as project metadata and duplicate canonical files; it is not a DB-to-archive reconciliation tool.",
                        "Re-run `am doctor fix --only fm-archive-state-files-archive-db-drift-anomalies --list` to confirm zero residual drift.",
                    ],
                    "warning": "Auto-fix is intentionally NOT implemented — picking the wrong authoritative side silently destroys data. The doctor requires operator judgment for this class.",
                    "common_causes": [
                        "Project row deleted from DB but archive directory not garbage-collected.",
                        "Archive restored from a backup older than the SQLite index.",
                        "Two STORAGE_ROOTs being mixed (e.g., test harness leaked into production root).",
                        "Half-applied schema migration that rebuilt one side without the other.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    pub storage_root_override: Option<PathBuf>,
    pub report_override: Option<ArchiveAnomalyReport>,
}

pub fn detect(inputs: &DetectInputs) -> Vec<ArchiveDbDriftFinding> {
    let report = match inputs.report_override.clone() {
        Some(r) => r,
        None => {
            let Some(root) = inputs.storage_root_override.clone() else {
                return Vec::new();
            };
            if !root.is_dir() {
                return Vec::new();
            }
            scan_archive_anomalies(&root)
        }
    };
    let mut project_mismatches: Vec<ProjectMismatchEntry> = Vec::new();
    let mut count_drifts: Vec<CountDriftEntry> = Vec::new();
    for a in &report.anomalies {
        match &a.kind {
            ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug,
                archive_human_key,
                detail,
            } => {
                project_mismatches.push(ProjectMismatchEntry {
                    archive_slug: archive_slug.clone(),
                    archive_human_key: archive_human_key.clone(),
                    detail: detail.clone(),
                });
            }
            ArchiveAnomalyKind::ArchiveDbCountDrift {
                archive_count,
                db_count,
                drift,
            } => {
                count_drifts.push(CountDriftEntry {
                    archive_count: *archive_count,
                    db_count: *db_count,
                    drift: *drift,
                });
            }
            _ => {}
        }
    }
    if project_mismatches.is_empty() && count_drifts.is_empty() {
        return Vec::new();
    }
    vec![ArchiveDbDriftFinding {
        project_mismatches,
        count_drifts,
    }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ArchiveDbDriftFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_db::archive_anomaly::ArchiveAnomaly;

    /// **NEGATIVE TEST FIRST**: empty report → no finding.
    #[test]
    fn detector_skips_clean_report() {
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(ArchiveAnomalyReport::new()),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: unrelated anomaly kinds → no finding here.
    /// Specifically ensures we don't accidentally pick up
    /// UnexpectedSymlink (FM7), SuspiciousEphemeralProject (FM5),
    /// or MissingProjectMetadata (FM6) — those are different FMs.
    #[test]
    fn detector_skips_report_with_only_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: Some("/tmp/x".to_string()),
                reason: "tmp-rooted".to_string(),
            },
        ));
        report
            .anomalies
            .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                path: "/x/link.md".into(),
                target: Some("/etc/passwd".into()),
            }));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_skips_when_no_inputs() {
        assert!(detect(&DetectInputs::default()).is_empty());
    }

    #[test]
    fn detector_skips_nonexistent_storage_root() {
        let inputs = DetectInputs {
            storage_root_override: Some("/nonexistent/path".into()),
            report_override: None,
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_single_project_mismatch() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug: "ghost_project".to_string(),
                archive_human_key: Some("/data/projects/ghost".to_string()),
                detail: "archive has slug `ghost_project`, no matching DB row".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].project_mismatches.len(), 1);
        assert!(findings[0].count_drifts.is_empty());
        assert_eq!(
            findings[0].project_mismatches[0].archive_slug,
            "ghost_project"
        );
    }

    #[test]
    fn detector_flags_single_count_drift() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbCountDrift {
                archive_count: 1500,
                db_count: 1200,
                drift: 300,
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].count_drifts.len(), 1);
        assert!(findings[0].project_mismatches.is_empty());
        assert_eq!(findings[0].count_drifts[0].drift, 300);
        assert_eq!(findings[0].count_drifts[0].archive_count, 1500);
        assert_eq!(findings[0].count_drifts[0].db_count, 1200);
    }

    #[test]
    fn detector_bundles_both_kinds_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug: "p1".to_string(),
                archive_human_key: None,
                detail: "no db row".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbCountDrift {
                archive_count: 10,
                db_count: 0,
                drift: 10,
            },
        ));
        // ALSO add an unrelated variant; it must be silently dropped.
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: None,
                reason: "tmp".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].project_mismatches.len(), 1);
        assert_eq!(findings[0].count_drifts.len(), 1);
        assert_eq!(findings[0].total_entries(), 2);
    }

    #[test]
    fn detector_aggregates_multiple_of_each_kind() {
        let mut report = ArchiveAnomalyReport::new();
        for i in 0..3 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                    archive_slug: format!("proj-{i}"),
                    archive_human_key: Some(format!("/x/{i}")),
                    detail: format!("d{i}"),
                },
            ));
        }
        for i in 0..2 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::ArchiveDbCountDrift {
                    archive_count: 100 + i,
                    db_count: 90 + i,
                    drift: 10,
                },
            ));
        }
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].project_mismatches.len(), 3);
        assert_eq!(findings[0].count_drifts.len(), 2);
        assert_eq!(findings[0].total_entries(), 5);
    }

    #[test]
    fn finding_serializes_with_warning_and_remediation() {
        let f = ArchiveDbDriftFinding {
            project_mismatches: vec![ProjectMismatchEntry {
                archive_slug: "p1".to_string(),
                archive_human_key: Some("/a/b".to_string()),
                detail: "no db row".to_string(),
            }],
            count_drifts: vec![CountDriftEntry {
                archive_count: 100,
                db_count: 50,
                drift: 50,
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("project_mismatches"));
        assert!(s.contains("count_drifts"));
        assert!(s.contains("warning"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am doctor archive-normalize --dry-run"));
        assert!(s.contains("am doctor reconstruct"));
        assert!(!s.contains("rewrites the archive from DB state"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        use std::fs;
        let td = tempfile::TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = ArchiveDbDriftFinding {
            project_mismatches: vec![],
            count_drifts: vec![],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
