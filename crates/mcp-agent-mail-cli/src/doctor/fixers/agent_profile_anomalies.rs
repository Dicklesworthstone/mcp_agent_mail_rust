//! `fm-archive-state-files-agent-profile-anomalies` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Each agent the archive knows about should have a
//! `<project_dir>/agents/<agent_name>/profile.json` with valid
//! JSON and the parent project also represented. Two ways
//! this can break:
//!
//! 1. **OrphanedAgentProfile**: agent dir exists under a
//!    project that the archive/DB doesn't recognize. Usually
//!    indicates manual `mv` operations or partial archive
//!    migration.
//! 2. **MalformedAgentProfile**: `profile.json` is missing or
//!    unparseable.
//!
//! Both surface in the TUI's Agents screen (blank entries,
//! "unknown project" labels) and break the
//! `am robot agents` JSON output.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters for the two agent-related variants.
//!
//! ## Fix
//!
//! **Detect-only.** For OrphanedAgentProfile, the operator
//! must decide if the parent project should be restored or
//! the orphan removed. For MalformedAgentProfile, the operator
//! must restore from a backup or rewrite the profile.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-agent-profile-anomalies";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentProfileProblem {
    Orphaned {
        profile_path: PathBuf,
        agent_name: String,
        parent_project_dir: PathBuf,
    },
    Malformed {
        profile_path: PathBuf,
        agent_name: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentProfileAnomaliesFinding {
    pub problems: Vec<AgentProfileProblem>,
}

impl AgentProfileAnomaliesFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n_orphaned = self
            .problems
            .iter()
            .filter(|p| matches!(p, AgentProfileProblem::Orphaned { .. }))
            .count();
        let n_malformed = self.problems.len() - n_orphaned;
        let title = format!(
            "{} agent profile anomaly(ies) in archive ({} orphaned, {} malformed)",
            self.problems.len(),
            n_orphaned,
            n_malformed,
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
                "orphaned_count": n_orphaned,
                "malformed_count": n_malformed,
                "manual_remediation": {
                    "steps": [
                        "For each Orphaned entry: decide whether the missing parent project should be restored from archive history (`git log -- projects/<slug>/`) or the orphan removed. Move orphan dirs into `.doctor/quarantine/orphan-agents/` if removing.",
                        "For each Malformed entry: inspect `detail` for the specific error (missing file, parse error, etc.). Restore from backup or rewrite `profile.json` with the canonical fields (agent_name, project_slug, program, model).",
                        "Re-run `am doctor --only fm-archive-state-files-agent-profile-anomalies` to confirm the agent surface is clean.",
                    ],
                    "note": "Auto-fix is intentionally not implemented — both variants require operator judgment about which canonical state to restore.",
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

pub fn detect(inputs: &DetectInputs) -> Vec<AgentProfileAnomaliesFinding> {
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
    let problems: Vec<AgentProfileProblem> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::OrphanedAgentProfile {
                profile_path,
                agent_name,
                parent_project_dir,
            } => Some(AgentProfileProblem::Orphaned {
                profile_path: profile_path.clone(),
                agent_name: agent_name.clone(),
                parent_project_dir: parent_project_dir.clone(),
            }),
            ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path,
                agent_name,
                detail,
            } => Some(AgentProfileProblem::Malformed {
                profile_path: profile_path.clone(),
                agent_name: agent_name.clone(),
                detail: detail.clone(),
            }),
            _ => None,
        })
        .collect();
    if problems.is_empty() {
        return Vec::new();
    }
    vec![AgentProfileAnomaliesFinding { problems }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &AgentProfileAnomaliesFinding,
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

    /// **NEGATIVE**: unrelated anomaly kinds → no finding here.
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
    fn detector_flags_orphaned_agent_profile() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::OrphanedAgentProfile {
                profile_path: "/x/projects/foo/agents/Alice/profile.json".into(),
                agent_name: "Alice".to_string(),
                parent_project_dir: "/x/projects/foo".into(),
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
            AgentProfileProblem::Orphaned { agent_name, .. } if agent_name == "Alice"
        ));
    }

    #[test]
    fn detector_flags_malformed_agent_profile() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path: "/x/agents/Bob/profile.json".into(),
                agent_name: "Bob".to_string(),
                detail: "parse error: unexpected end of input".to_string(),
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
            AgentProfileProblem::Malformed { detail, .. } if detail.contains("parse error")
        ));
    }

    #[test]
    fn detector_aggregates_mixed_variants_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::OrphanedAgentProfile {
                profile_path: "/x/agents/a/profile.json".into(),
                agent_name: "a".to_string(),
                parent_project_dir: "/x/projects/gone".into(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path: "/x/agents/b/profile.json".into(),
                agent_name: "b".to_string(),
                detail: "missing file".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::OrphanedAgentProfile {
                profile_path: "/x/agents/c/profile.json".into(),
                agent_name: "c".to_string(),
                parent_project_dir: "/x/projects/gone2".into(),
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
        let f = AgentProfileAnomaliesFinding {
            problems: vec![
                AgentProfileProblem::Orphaned {
                    profile_path: "/x/a.json".into(),
                    agent_name: "a".to_string(),
                    parent_project_dir: "/x/gone".into(),
                },
                AgentProfileProblem::Orphaned {
                    profile_path: "/x/b.json".into(),
                    agent_name: "b".to_string(),
                    parent_project_dir: "/x/gone".into(),
                },
                AgentProfileProblem::Malformed {
                    profile_path: "/x/c.json".into(),
                    agent_name: "c".to_string(),
                    detail: "bad".to_string(),
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"orphaned_count\":2"));
        assert!(s.contains("\"malformed_count\":1"));
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
        let finding = AgentProfileAnomaliesFinding { problems: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
