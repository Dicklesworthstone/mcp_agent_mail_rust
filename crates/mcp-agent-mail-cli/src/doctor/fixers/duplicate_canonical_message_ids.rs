//! `fm-archive-state-files-duplicate-canonical-message-ids` —
//! P0 detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Each message in the mailbox archive must resolve to a
//! unique positive `message_id`. Duplicate canonical IDs
//! across `.md` files break:
//! - Message-thread reconstruction (`am robot thread <id>`
//!   returns one body but the archive has two).
//! - Ack accounting (the second copy never gets its ack
//!   recorded).
//! - Cross-project search (FTS V3 indexes by ID).
//! - `am doctor reconstruct` — would conflict on insert.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters for `ArchiveAnomalyKind::DuplicateCanonicalId`.
//! Mirrors the FM5 / FM6 / FM7 anomaly-wrapper pattern.
//!
//! ## Fix
//!
//! **Detect-only.** Choosing WHICH duplicate to keep is
//! semantic — `scan_archive_anomalies` notes `keep_path`
//! (first encountered) but operators may prefer the
//! lexicographically-latest copy or the one with the most
//! recent mtime. Manual remediation walks the operator through
//! the per-duplicate triage and quarantine flow.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-duplicate-canonical-message-ids";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateEntry {
    pub message_id: i64,
    pub keep_path: PathBuf,
    pub duplicate_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCanonicalMessageIdsFinding {
    pub duplicates: Vec<DuplicateEntry>,
}

impl DuplicateCanonicalMessageIdsFinding {
    pub fn to_finding(&self) -> super::Finding {
        let total_dupes: usize = self
            .duplicates
            .iter()
            .map(|d| d.duplicate_paths.len())
            .sum();
        let title = format!(
            "{} message_id(s) have duplicate archive files ({} duplicate file(s) total); breaks thread reconstruction + ack accounting",
            self.duplicates.len(),
            total_dupes,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "duplicates": self.duplicates,
                "distinct_id_count": self.duplicates.len(),
                "duplicate_file_count": total_dupes,
                "manual_remediation": {
                    "steps": [
                        "For each duplicate: `diff <keep_path> <duplicate_paths[i]>` to confirm they're byte-identical (if so, the duplicate is safe to quarantine).",
                        "If the duplicates differ in content (e.g., one has updated frontmatter), pick the canonical version per the project's data governance policy — typically the most-recent-mtime copy.",
                        "Move non-canonical copies into a quarantine dir: `mkdir -p .doctor/quarantine/dup-message-ids && mv <duplicate_path> .doctor/quarantine/dup-message-ids/`.",
                        "Re-run `am doctor fix --only fm-archive-state-files-duplicate-canonical-message-ids --list` to confirm the archive is unique.",
                        "Optionally run `am doctor reconstruct` to rebuild the SQLite index from the deduplicated archive.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — choosing which duplicate to keep is semantic (content vs. mtime vs. lexicographic order).",
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

pub fn detect(inputs: &DetectInputs) -> Vec<DuplicateCanonicalMessageIdsFinding> {
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
    let duplicates: Vec<DuplicateEntry> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::DuplicateCanonicalId {
                message_id,
                keep_path,
                duplicate_paths,
            } => Some(DuplicateEntry {
                message_id: *message_id,
                keep_path: keep_path.clone(),
                duplicate_paths: duplicate_paths.clone(),
            }),
            _ => None,
        })
        .collect();
    if duplicates.is_empty() {
        return Vec::new();
    }
    vec![DuplicateCanonicalMessageIdsFinding { duplicates }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &DuplicateCanonicalMessageIdsFinding,
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
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: Some("/tmp/x".to_string()),
                reason: "tmp".to_string(),
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
    fn detector_flags_single_duplicate() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::DuplicateCanonicalId {
                message_id: 42,
                keep_path: "/x/foo.md".into(),
                duplicate_paths: vec!["/x/foo-dup.md".into()],
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].duplicates.len(), 1);
        assert_eq!(findings[0].duplicates[0].message_id, 42);
        assert_eq!(findings[0].duplicates[0].duplicate_paths.len(), 1);
    }

    #[test]
    fn detector_flags_multiple_duplicates_per_id() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::DuplicateCanonicalId {
                message_id: 42,
                keep_path: "/x/foo.md".into(),
                duplicate_paths: vec![
                    "/x/foo-dup1.md".into(),
                    "/x/foo-dup2.md".into(),
                    "/x/foo-dup3.md".into(),
                ],
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].duplicates[0].duplicate_paths.len(), 3);
    }

    #[test]
    fn detector_aggregates_multiple_ids_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        for i in 1..=3 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::DuplicateCanonicalId {
                    message_id: i,
                    keep_path: format!("/x/msg-{i}.md").into(),
                    duplicate_paths: vec![format!("/x/msg-{i}-dup.md").into()],
                },
            ));
        }
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].duplicates.len(), 3);
    }

    #[test]
    fn finding_serializes_with_counts() {
        let f = DuplicateCanonicalMessageIdsFinding {
            duplicates: vec![
                DuplicateEntry {
                    message_id: 1,
                    keep_path: "/x/a.md".into(),
                    duplicate_paths: vec!["/x/a-dup.md".into(), "/x/a-dup2.md".into()],
                },
                DuplicateEntry {
                    message_id: 2,
                    keep_path: "/x/b.md".into(),
                    duplicate_paths: vec!["/x/b-dup.md".into()],
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"distinct_id_count\":2"));
        assert!(s.contains("\"duplicate_file_count\":3"));
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
        let finding = DuplicateCanonicalMessageIdsFinding { duplicates: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
