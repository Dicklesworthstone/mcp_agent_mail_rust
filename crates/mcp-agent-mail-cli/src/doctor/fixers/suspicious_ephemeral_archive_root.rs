//! `fm-archive-state-files-suspicious-ephemeral-archive-root` — P3
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Test runs and one-shot scripts sometimes seed agent-mail
//! projects rooted at `/tmp/...`, `/var/tmp/...`, or
//! `.../tmp-XXXX/` paths. These ephemeral roots leak into the
//! global mailbox archive at
//! `~/.mcp_agent_mail_git_mailbox_repo/projects/<slug>/` and
//! stay there forever — the archive accumulates project entries
//! that no longer correspond to real working directories. This
//! is informational P3, not a hard failure.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters its report for
//! `ArchiveAnomalyKind::SuspiciousEphemeralProject`. Each
//! matching anomaly becomes one finding entry.
//!
//! ## Fix
//!
//! **Detect-only.** The repair_spec describes a heavily-gated
//! Op::Rename-to-quarantine fixer (`--yes --before <date>` plus
//! capability flags). Wiring that through the chokepoint is
//! substantial scope and intentionally deferred. Manual
//! remediation points operators at
//! `am doctor archive-normalize --dry-run` and the standard
//! per-project quarantine workflow.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-suspicious-ephemeral-archive-root";
const FM_SEVERITY: &str = "P3";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct SuspiciousEphemeralEntry {
    pub project_dir: PathBuf,
    pub slug: String,
    pub human_key: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuspiciousEphemeralFinding {
    pub entries: Vec<SuspiciousEphemeralEntry>,
}

impl SuspiciousEphemeralFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "global mailbox archive contains {} suspicious ephemeral project root(s); manual quarantine recommended",
            self.entries.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.85,
            evidence: serde_json::json!({
                "ephemerals": self.entries,
                "manual_remediation": {
                    "steps": [
                        "Inspect the listed project_dirs. If they are genuinely ephemeral / leftover from test runs, quarantine them via `am doctor archive-normalize --dry-run` (preview) then `am doctor archive-normalize --yes`.",
                        "To prevent future occurrences, set `STORAGE_ROOT` to an isolated path for tests / one-shot scripts (avoids polluting the global mailbox).",
                        "If `ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE=true` is intentionally set, this FM can be safely ignored.",
                    ],
                    "note": "Op::Rename-to-quarantine auto-fix is intentionally deferred — needs operator-supplied `--before <date>` gate and capability flags.",
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

/// Inputs for the detector. Production callers leave both fields
/// as defaults; tests inject a fabricated report.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    pub storage_root_override: Option<PathBuf>,
    pub report_override: Option<ArchiveAnomalyReport>,
}

pub fn detect(inputs: &DetectInputs) -> Vec<SuspiciousEphemeralFinding> {
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
    let entries: Vec<SuspiciousEphemeralEntry> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir,
                slug,
                human_key,
                reason,
            } => Some(SuspiciousEphemeralEntry {
                project_dir: project_dir.clone(),
                slug: slug.clone(),
                human_key: human_key.clone(),
                reason: reason.clone(),
            }),
            _ => None,
        })
        .collect();

    if entries.is_empty() {
        return Vec::new();
    }
    vec![SuspiciousEphemeralFinding { entries }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &SuspiciousEphemeralFinding,
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

    /// **NEGATIVE TEST FIRST** (pass-35V lesson): a clean report
    /// (no SuspiciousEphemeralProject entries) → no finding.
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

    /// **NEGATIVE TEST**: report has other anomaly kinds but
    /// none are SuspiciousEphemeralProject → no finding.
    #[test]
    fn detector_skips_report_with_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MalformedAgentProfile {
                profile_path: "/x/profile.json".into(),
                agent_name: "Alice".into(),
                detail: "parse error".into(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "non-SuspiciousEphemeralProject anomalies must not be flagged here"
        );
    }

    /// **NEGATIVE TEST**: no report override + no storage_root
    /// override → empty result (don't crash).
    #[test]
    fn detector_skips_when_no_inputs() {
        let inputs = DetectInputs::default();
        let findings = detect(&inputs);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE TEST**: storage_root override points at a
    /// nonexistent dir → no finding.
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
    fn detector_flags_one_ephemeral_per_report() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::SuspiciousEphemeralProject {
                project_dir: "/tmp/test-run-abc".into(),
                slug: "test-run-abc".to_string(),
                human_key: Some("/tmp/test-run-abc".to_string()),
                reason: "rooted under /tmp".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 1);
        assert_eq!(findings[0].entries[0].slug, "test-run-abc");
    }

    #[test]
    fn detector_aggregates_multiple_ephemerals_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        for (path, slug) in &[("/tmp/a", "a"), ("/var/tmp/b", "b"), ("/tmp/c", "c")] {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::SuspiciousEphemeralProject {
                    project_dir: (*path).into(),
                    slug: (*slug).to_string(),
                    human_key: None,
                    reason: "ephemeral".to_string(),
                },
            ));
        }
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        // Single finding aggregates all entries (matches the
        // repair_spec's "1 finding, N entries" shape).
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[test]
    fn finding_serializes_with_entry_list_and_remediation() {
        let f = SuspiciousEphemeralFinding {
            entries: vec![SuspiciousEphemeralEntry {
                project_dir: "/tmp/x".into(),
                slug: "x".to_string(),
                human_key: Some("/tmp/x".to_string()),
                reason: "rooted under /tmp".to_string(),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"slug\":\"x\""));
        assert!(s.contains("manual_remediation"));
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
        let finding = SuspiciousEphemeralFinding { entries: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
