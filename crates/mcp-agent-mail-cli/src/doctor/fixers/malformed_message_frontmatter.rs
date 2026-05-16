//! `fm-archive-state-files-malformed-message-frontmatter` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Every `.md` message file under `messages/YYYY/MM/` must
//! start with a JSON frontmatter block containing:
//! - `id` (positive integer)
//! - `from`, `to`, `subject` (string fields)
//! - A created timestamp
//!
//! Four ways the frontmatter can be broken:
//! 1. **MissingFrontmatter**: no JSON block at all.
//! 2. **UnparseableFrontmatter**: block exists but fails JSON
//!    parsing.
//! 3. **InvalidMessageId**: parses but `id` is missing /
//!    zero / negative / non-integer.
//! 4. **IncompleteFrontmatter**: parses with valid id but
//!    other required fields are missing / malformed.
//!
//! Any of these breaks the canonical-message-id mapping used
//! by `am robot thread <id>`, ack accounting, FTS V3 search
//! indexing, and `am doctor reconstruct`.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters for the 4 frontmatter variants. Mirrors the
//! FM5 / FM6 / FM7 / FM9 anomaly-wrapper pattern.
//!
//! ## Fix
//!
//! **Detect-only.** Repairing frontmatter requires
//! operator-supplied truth (the canonical message_id, from,
//! to, subject, timestamp values). Manual remediation walks
//! the operator through inspect → fix → reparse → verify.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-malformed-message-frontmatter";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrontmatterProblem {
    Missing {
        path: PathBuf,
    },
    Unparseable {
        path: PathBuf,
        parse_error: String,
    },
    InvalidMessageId {
        path: PathBuf,
        detail: String,
    },
    Incomplete {
        path: PathBuf,
        missing_fields: Vec<String>,
    },
}

impl FrontmatterProblem {
    fn path(&self) -> &PathBuf {
        match self {
            FrontmatterProblem::Missing { path }
            | FrontmatterProblem::Unparseable { path, .. }
            | FrontmatterProblem::InvalidMessageId { path, .. }
            | FrontmatterProblem::Incomplete { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MalformedMessageFrontmatterFinding {
    pub problems: Vec<FrontmatterProblem>,
}

impl MalformedMessageFrontmatterFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n_missing = self
            .problems
            .iter()
            .filter(|p| matches!(p, FrontmatterProblem::Missing { .. }))
            .count();
        let n_unparseable = self
            .problems
            .iter()
            .filter(|p| matches!(p, FrontmatterProblem::Unparseable { .. }))
            .count();
        let n_invalid_id = self
            .problems
            .iter()
            .filter(|p| matches!(p, FrontmatterProblem::InvalidMessageId { .. }))
            .count();
        let n_incomplete = self
            .problems
            .iter()
            .filter(|p| matches!(p, FrontmatterProblem::Incomplete { .. }))
            .count();
        let title = format!(
            "{} message file(s) have malformed frontmatter ({} missing, {} unparseable, {} invalid-id, {} incomplete)",
            self.problems.len(),
            n_missing,
            n_unparseable,
            n_invalid_id,
            n_incomplete,
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
                "missing_count": n_missing,
                "unparseable_count": n_unparseable,
                "invalid_id_count": n_invalid_id,
                "incomplete_count": n_incomplete,
                "manual_remediation": {
                    "steps": [
                        "For each Missing entry: the file has no JSON frontmatter block. Add one with the canonical id/from/to/subject/timestamp fields.",
                        "For each Unparseable entry: read the `parse_error` field for the specific JSON error. Fix the syntax.",
                        "For each InvalidMessageId entry: read `detail` (missing/zero/negative). Restore the correct positive integer id from a backup or the SQLite mirror.",
                        "For each Incomplete entry: read `missing_fields` and fill in each one.",
                        "After all fixes: `am doctor reconstruct --project <slug>` to rebuild the SQLite index from the now-clean archive.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — frontmatter content requires operator-supplied truth (which positive id corresponds to this file, who the original sender was, etc.).",
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

pub fn detect(inputs: &DetectInputs) -> Vec<MalformedMessageFrontmatterFinding> {
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
    let problems: Vec<FrontmatterProblem> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::MissingFrontmatter { path } => {
                Some(FrontmatterProblem::Missing { path: path.clone() })
            }
            ArchiveAnomalyKind::UnparseableFrontmatter { path, parse_error } => {
                Some(FrontmatterProblem::Unparseable {
                    path: path.clone(),
                    parse_error: parse_error.clone(),
                })
            }
            ArchiveAnomalyKind::InvalidMessageId { path, detail } => {
                Some(FrontmatterProblem::InvalidMessageId {
                    path: path.clone(),
                    detail: detail.clone(),
                })
            }
            ArchiveAnomalyKind::IncompleteFrontmatter {
                path,
                missing_fields,
            } => Some(FrontmatterProblem::Incomplete {
                path: path.clone(),
                missing_fields: missing_fields.clone(),
            }),
            _ => None,
        })
        .collect();
    if problems.is_empty() {
        return Vec::new();
    }
    vec![MalformedMessageFrontmatterFinding { problems }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &MalformedMessageFrontmatterFinding,
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
            ArchiveAnomalyKind::DuplicateCanonicalId {
                message_id: 1,
                keep_path: "/x.md".into(),
                duplicate_paths: vec!["/y.md".into()],
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
    fn detector_flags_missing_frontmatter() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingFrontmatter {
                path: "/x/2026/05/msg1.md".into(),
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
            FrontmatterProblem::Missing { .. }
        ));
    }

    #[test]
    fn detector_flags_unparseable_frontmatter() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::UnparseableFrontmatter {
                path: "/x.md".into(),
                parse_error: "expected value at line 1 column 1".to_string(),
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
            FrontmatterProblem::Unparseable { parse_error, .. } if parse_error.contains("expected")
        ));
    }

    #[test]
    fn detector_flags_invalid_message_id() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidMessageId {
                path: "/x.md".into(),
                detail: "id is zero".to_string(),
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
            FrontmatterProblem::InvalidMessageId { detail, .. } if detail == "id is zero"
        ));
    }

    #[test]
    fn detector_flags_incomplete_frontmatter() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::IncompleteFrontmatter {
                path: "/x.md".into(),
                missing_fields: vec!["from".to_string(), "subject".to_string()],
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
            FrontmatterProblem::Incomplete { missing_fields, .. } if missing_fields.len() == 2
        ));
    }

    #[test]
    fn detector_aggregates_mixed_variants_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingFrontmatter {
                path: "/a.md".into(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::UnparseableFrontmatter {
                path: "/b.md".into(),
                parse_error: "bad".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidMessageId {
                path: "/c.md".into(),
                detail: "negative".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::IncompleteFrontmatter {
                path: "/d.md".into(),
                missing_fields: vec!["subject".to_string()],
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 4);
    }

    #[test]
    fn finding_serializes_with_per_variant_breakdown() {
        let f = MalformedMessageFrontmatterFinding {
            problems: vec![
                FrontmatterProblem::Missing {
                    path: "/a.md".into(),
                },
                FrontmatterProblem::Missing {
                    path: "/b.md".into(),
                },
                FrontmatterProblem::Unparseable {
                    path: "/c.md".into(),
                    parse_error: "bad".to_string(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"missing_count\":2"));
        assert!(s.contains("\"unparseable_count\":1"));
        assert!(s.contains("\"invalid_id_count\":0"));
        assert!(s.contains("\"incomplete_count\":0"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    /// Bonus: enum-variant path() accessor consistency.
    #[test]
    fn frontmatter_problem_path_accessor() {
        let p: PathBuf = "/x.md".into();
        let cases = [
            FrontmatterProblem::Missing { path: p.clone() },
            FrontmatterProblem::Unparseable {
                path: p.clone(),
                parse_error: "".to_string(),
            },
            FrontmatterProblem::InvalidMessageId {
                path: p.clone(),
                detail: "".to_string(),
            },
            FrontmatterProblem::Incomplete {
                path: p.clone(),
                missing_fields: vec![],
            },
        ];
        for c in &cases {
            assert_eq!(c.path(), &p);
        }
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
        let finding = MalformedMessageFrontmatterFinding { problems: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
