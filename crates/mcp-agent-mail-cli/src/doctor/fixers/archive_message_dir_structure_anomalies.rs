//! `fm-archive-state-files-archive-message-dir-structure-anomalies` —
//! P2 detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! The `<project>/messages/` directory must follow a strict
//! `YYYY/MM/<id>.md` layout. Two structural anomalies break the
//! archive walker:
//!
//! 1. **InvalidDateDirectory**: a year-level or month-level dir
//!    has an unexpected name (not 4-digit year or 2-digit
//!    month). E.g., `messages/2026/13/` (month > 12) or
//!    `messages/draft/`.
//! 2. **UnexpectedFileInMessageDir**: a non-`.md` file lives in
//!    `messages/YYYY/MM/`. Could be a stray `.swp` editor
//!    backup, `.DS_Store`, `Thumbs.db`, or a misnamed message.
//!
//! Both prevent FTS V3 indexing and break archive replay
//! (`am doctor reconstruct` skips non-canonical entries).
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters for these two variants.
//!
//! ## Fix
//!
//! **Detect-only.** Operators decide per-anomaly: rename the
//! invalid date dir to its canonical equivalent, or quarantine
//! the unexpected file.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, DateDirectoryLevel, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-archive-message-dir-structure-anomalies";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructureProblem {
    InvalidDateDirectory {
        path: PathBuf,
        level: DateDirectoryLevel,
        name: String,
    },
    UnexpectedFileInMessageDir {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveMessageDirStructureAnomaliesFinding {
    pub problems: Vec<StructureProblem>,
}

impl ArchiveMessageDirStructureAnomaliesFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n_invalid_dir = self
            .problems
            .iter()
            .filter(|p| matches!(p, StructureProblem::InvalidDateDirectory { .. }))
            .count();
        let n_unexpected_file = self.problems.len() - n_invalid_dir;
        let title = format!(
            "{} message-dir structure anomaly(ies) ({} invalid date dirs, {} unexpected files)",
            self.problems.len(),
            n_invalid_dir,
            n_unexpected_file,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "problems": self.problems,
                "total_count": self.problems.len(),
                "invalid_date_dir_count": n_invalid_dir,
                "unexpected_file_count": n_unexpected_file,
                "manual_remediation": {
                    "steps": [
                        "For each InvalidDateDirectory: rename the directory to its canonical 4-digit-year / 2-digit-month form, OR if the content doesn't fit the date pattern (e.g., a `draft/` dir), move it OUT of `messages/`.",
                        "For each UnexpectedFileInMessageDir: editor swap files (`.swp`), OS metadata (`.DS_Store`, `Thumbs.db`), and similar artifacts should be removed. A misnamed message can be renamed to `<id>.md` after confirming the canonical id.",
                        "After fixing structural anomalies, re-run `am doctor reconstruct` to refresh the SQLite index from the now-clean archive.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — renaming directories or removing files needs operator confirmation about which transformations preserve message identity.",
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

pub fn detect(inputs: &DetectInputs) -> Vec<ArchiveMessageDirStructureAnomaliesFinding> {
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
    let problems: Vec<StructureProblem> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::InvalidDateDirectory { path, level, name } => {
                Some(StructureProblem::InvalidDateDirectory {
                    path: path.clone(),
                    level: *level,
                    name: name.clone(),
                })
            }
            ArchiveAnomalyKind::UnexpectedFileInMessageDir { path } => {
                Some(StructureProblem::UnexpectedFileInMessageDir { path: path.clone() })
            }
            _ => None,
        })
        .collect();
    if problems.is_empty() {
        return Vec::new();
    }
    vec![ArchiveMessageDirStructureAnomaliesFinding { problems }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ArchiveMessageDirStructureAnomaliesFinding,
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
        assert!(detect(&inputs).is_empty());
    }

    /// **NEGATIVE**: unrelated anomalies don't surface here.
    #[test]
    fn detector_skips_report_with_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingFrontmatter {
                path: "/x.md".into(),
            },
        ));
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
            storage_root_override: Some("/nonexistent".into()),
            report_override: None,
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_invalid_year_directory() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidDateDirectory {
                path: "/x/projects/foo/messages/draft".into(),
                level: DateDirectoryLevel::Year,
                name: "draft".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            StructureProblem::InvalidDateDirectory { level, name, .. }
                if *level == DateDirectoryLevel::Year && name == "draft"
        ));
    }

    #[test]
    fn detector_flags_invalid_month_directory() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidDateDirectory {
                path: "/x/messages/2026/13".into(),
                level: DateDirectoryLevel::Month,
                name: "13".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            StructureProblem::InvalidDateDirectory { level, .. }
                if *level == DateDirectoryLevel::Month
        ));
    }

    #[test]
    fn detector_flags_unexpected_file_in_message_dir() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::UnexpectedFileInMessageDir {
                path: "/x/messages/2026/05/.DS_Store".into(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            StructureProblem::UnexpectedFileInMessageDir { .. }
        ));
    }

    #[test]
    fn detector_aggregates_mixed_variants_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidDateDirectory {
                path: "/x/messages/9999".into(),
                level: DateDirectoryLevel::Year,
                name: "9999".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::UnexpectedFileInMessageDir {
                path: "/x/messages/2026/05/.DS_Store".into(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::UnexpectedFileInMessageDir {
                path: "/x/messages/2026/05/foo.swp".into(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 3);
    }

    #[test]
    fn finding_serializes_with_per_variant_counts() {
        let f = ArchiveMessageDirStructureAnomaliesFinding {
            problems: vec![
                StructureProblem::InvalidDateDirectory {
                    path: "/x/messages/draft".into(),
                    level: DateDirectoryLevel::Year,
                    name: "draft".to_string(),
                },
                StructureProblem::UnexpectedFileInMessageDir {
                    path: "/x/messages/2026/05/.DS_Store".into(),
                },
                StructureProblem::UnexpectedFileInMessageDir {
                    path: "/x/messages/2026/05/foo.swp".into(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"invalid_date_dir_count\":1"));
        assert!(s.contains("\"unexpected_file_count\":2"));
        assert!(s.contains("\"auto_fixable\":false"));
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
        let finding = ArchiveMessageDirStructureAnomaliesFinding { problems: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
