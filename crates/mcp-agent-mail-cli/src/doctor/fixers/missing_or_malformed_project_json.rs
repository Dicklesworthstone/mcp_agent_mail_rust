//! `fm-archive-state-files-missing-or-malformed-project-json` —
//! P1 detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Every `<storage_root>/projects/<slug>/` directory must
//! contain a `project.json` file with valid JSON and the required
//! `slug` + `human_key` fields. Missing or malformed metadata
//! breaks many downstream paths: the TUI's projects screen
//! shows blank entries; `am robot status` returns errors;
//! archive replay can't reconstruct the project identity.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters its report for `MissingProjectMetadata` OR
//! `InvalidProjectMetadata`. Mirrors the FM5 pattern (also a
//! `scan_archive_anomalies` wrapper) — see
//! `suspicious_ephemeral_archive_root.rs` for the same shape.
//!
//! ## Fix
//!
//! **Detect-only.** Writing a corrected `project.json` requires
//! operator-supplied truth (the right slug + human_key
//! correspondence) and is intentionally outside the chokepoint's
//! scope. Manual remediation walks operators through:
//! 1. Inspect `project_dir` to identify the intended project.
//! 2. Write a minimal `project.json` with `slug` and `human_key`.
//! 3. Re-run the detector to confirm the anomaly is gone.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-missing-or-malformed-project-json";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectJsonProblem {
    /// `project.json` does not exist in the project directory.
    Missing {
        project_dir: PathBuf,
        fallback_slug: String,
    },
    /// `project.json` exists but is invalid JSON or missing
    /// required fields.
    Invalid {
        path: PathBuf,
        slug: String,
        canonical_human_key: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingOrMalformedProjectJsonFinding {
    pub problems: Vec<ProjectJsonProblem>,
}

impl MissingOrMalformedProjectJsonFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n_missing = self
            .problems
            .iter()
            .filter(|p| matches!(p, ProjectJsonProblem::Missing { .. }))
            .count();
        let n_invalid = self.problems.len() - n_missing;
        let title = format!(
            "{} project(s) in archive have missing or malformed `project.json` ({} missing, {} invalid)",
            self.problems.len(),
            n_missing,
            n_invalid,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "problems": self.problems,
                "missing_count": n_missing,
                "invalid_count": n_invalid,
                "manual_remediation": {
                    "steps": [
                        "For each Missing entry: inspect `project_dir` to identify the intended project. Create a minimal `project.json` with `slug` (the directory's basename) and `human_key` (the project's canonical filesystem path).",
                        "For each Invalid entry: read the `detail` field for the specific JSON / schema error. Edit `path` to fix the JSON or add the missing required field.",
                        "Re-run `am doctor fix --only fm-archive-state-files-missing-or-malformed-project-json --list` to confirm the anomaly is gone.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — writing a project.json requires operator-supplied truth (the right slug+human_key correspondence).",
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

pub fn detect(inputs: &DetectInputs) -> Vec<MissingOrMalformedProjectJsonFinding> {
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
    let problems: Vec<ProjectJsonProblem> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir,
                fallback_slug,
            } => Some(ProjectJsonProblem::Missing {
                project_dir: project_dir.clone(),
                fallback_slug: fallback_slug.clone(),
            }),
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path,
                slug,
                canonical_human_key,
                detail,
            } => Some(ProjectJsonProblem::Invalid {
                path: path.clone(),
                slug: slug.clone(),
                canonical_human_key: canonical_human_key.clone(),
                detail: detail.clone(),
            }),
            _ => None,
        })
        .collect();
    if problems.is_empty() {
        return Vec::new();
    }
    vec![MissingOrMalformedProjectJsonFinding { problems }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &MissingOrMalformedProjectJsonFinding,
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

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): empty report
    /// → no finding.
    #[test]
    fn detector_skips_clean_report() {
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(ArchiveAnomalyReport::new()),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "empty anomaly report must not emit a finding"
        );
    }

    /// **NEGATIVE TEST**: report has unrelated anomalies (e.g.,
    /// SuspiciousEphemeralProject — that's FM5's domain) → no
    /// finding from this FM.
    #[test]
    fn detector_skips_report_with_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: Some("/tmp/x".to_string()),
                reason: "tmp-rooted".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "SuspiciousEphemeralProject is FM5's domain; must not surface here"
        );
    }

    #[test]
    fn detector_skips_when_no_inputs() {
        let inputs = DetectInputs::default();
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_nonexistent_storage_root() {
        let inputs = DetectInputs {
            storage_root_override: Some("/nonexistent/path".into()),
            report_override: None,
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_missing_project_metadata() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/var/data/projects/foo".into(),
                fallback_slug: "foo".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            ProjectJsonProblem::Missing { fallback_slug, .. } if fallback_slug == "foo"
        ));
    }

    #[test]
    fn detector_flags_invalid_project_metadata() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path: "/var/data/projects/foo/project.json".into(),
                slug: "foo".to_string(),
                canonical_human_key: None,
                detail: "malformed JSON: expected value at line 1 column 1".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].problems.len(), 1);
        assert!(matches!(
            &findings[0].problems[0],
            ProjectJsonProblem::Invalid { detail, .. } if detail.contains("malformed JSON")
        ));
    }

    #[test]
    fn detector_aggregates_mixed_problems_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/p/a".into(),
                fallback_slug: "a".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::InvalidProjectMetadata {
                path: "/p/b/project.json".into(),
                slug: "b".to_string(),
                canonical_human_key: Some("/work/b".to_string()),
                detail: "missing slug field".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingProjectMetadata {
                project_dir: "/p/c".into(),
                fallback_slug: "c".to_string(),
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
    fn finding_serializes_with_problem_breakdown() {
        let f = MissingOrMalformedProjectJsonFinding {
            problems: vec![
                ProjectJsonProblem::Missing {
                    project_dir: "/p/a".into(),
                    fallback_slug: "a".to_string(),
                },
                ProjectJsonProblem::Invalid {
                    path: "/p/b/project.json".into(),
                    slug: "b".to_string(),
                    canonical_human_key: None,
                    detail: "bad json".to_string(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"missing_count\":1"));
        assert!(s.contains("\"invalid_count\":1"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("manual_remediation"));
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
        let finding = MissingOrMalformedProjectJsonFinding { problems: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
