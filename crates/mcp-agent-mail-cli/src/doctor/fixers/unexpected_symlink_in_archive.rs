//! `fm-archive-state-files-unexpected-symlink-in-archive` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! The mailbox archive should consist of regular files and
//! directories only. Symlinks inside the archive are never
//! canonical — they may indicate filesystem-level tampering
//! (an attacker pointing `<storage_root>/.../foo.md` at
//! `/etc/shadow` to exfiltrate via `am robot thread <id>`),
//! a misconfigured storage migration that aliased files via
//! symlinks rather than copying, or a manual operator edit.
//!
//! ## Detection
//!
//! Wraps `mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies(...)`
//! and filters for `ArchiveAnomalyKind::UnexpectedSymlink`.
//! Mirrors the FM5 / FM6 pattern.
//!
//! ## Fix
//!
//! **Detect-only.** The doctor cannot safely remove a symlink
//! without operator confirmation — the target may be data the
//! operator intentionally aliased, and the chokepoint's
//! Op::Rename quarantine path is gated on regular-file
//! semantics. Manual remediation walks operators through:
//!
//! 1. Inspect each `path` to identify the intended content.
//! 2. If the symlink target is the canonical source, replace the
//!    symlink with a copy of the target.
//! 3. If the symlink is unintentional (attacker insertion,
//!    bad migration), move it to quarantine after preserving
//!    any target bytes that matter.
//! 4. Re-run the detector to confirm the archive is clean.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-unexpected-symlink-in-archive";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct UnexpectedSymlinkEntry {
    pub path: PathBuf,
    pub target: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnexpectedSymlinkFinding {
    pub entries: Vec<UnexpectedSymlinkEntry>,
}

impl UnexpectedSymlinkFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} unexpected symlink(s) in mailbox archive (possible filesystem tampering or misconfigured storage)",
            self.entries.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "symlinks": self.entries,
                "count": self.entries.len(),
                "manual_remediation": {
                    "steps": [
                        "For each path: `ls -la <path>` to inspect the symlink + target.",
                        "If the target is the canonical source the archive should point at: move the symlink into `.doctor/quarantine/archive-symlinks/`, then `cp <target> <path>` to recreate the archive entry as a regular file.",
                        "If the symlink is unintentional (post-migration artifact, attacker insertion): preserve any target bytes that matter, then move the symlink into `.doctor/quarantine/archive-symlinks/`.",
                        "Re-run `am doctor fix --only fm-archive-state-files-unexpected-symlink-in-archive --list` to confirm the archive is clean.",
                    ],
                    "warning": "Symlinks in the archive can be a SECURITY signal (attacker aliasing archive files at sensitive system paths). Investigate any target outside `<storage_root>` carefully BEFORE quarantining the symlink.",
                    "note": "Auto-fix is intentionally not implemented — symlink semantics require operator judgment.",
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

pub fn detect(inputs: &DetectInputs) -> Vec<UnexpectedSymlinkFinding> {
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
    let entries: Vec<UnexpectedSymlinkEntry> = report
        .anomalies
        .iter()
        .filter_map(|a| match &a.kind {
            ArchiveAnomalyKind::UnexpectedSymlink { path, target } => {
                Some(UnexpectedSymlinkEntry {
                    path: path.clone(),
                    target: target.clone(),
                })
            }
            _ => None,
        })
        .collect();
    if entries.is_empty() {
        return Vec::new();
    }
    vec![UnexpectedSymlinkFinding { entries }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &UnexpectedSymlinkFinding,
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
    fn detector_flags_unexpected_symlink_with_target() {
        let mut report = ArchiveAnomalyReport::new();
        report
            .anomalies
            .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                path: "/var/data/projects/foo/2026-05/bar.md".into(),
                target: Some("/etc/passwd".into()),
            }));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 1);
        assert_eq!(
            findings[0].entries[0]
                .target
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn detector_flags_unexpected_symlink_without_target() {
        let mut report = ArchiveAnomalyReport::new();
        report
            .anomalies
            .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                path: "/var/data/projects/foo/2026-05/dangling.md".into(),
                target: None,
            }));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].target, None);
    }

    #[test]
    fn detector_aggregates_multiple_symlinks_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        for i in 0..3 {
            report
                .anomalies
                .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                    path: format!("/x/link-{i}.md").into(),
                    target: Some(format!("/y/target-{i}").into()),
                }));
        }
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[test]
    fn finding_serializes_with_count_and_warning() {
        let f = UnexpectedSymlinkFinding {
            entries: vec![UnexpectedSymlinkEntry {
                path: "/x/link.md".into(),
                target: Some("/etc/shadow".into()),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"count\":1"));
        assert!(s.contains("warning"));
        assert!(s.contains("\"auto_fixable\":false"));
        // Target path appears in evidence (for operator visibility).
        assert!(s.contains("shadow"));
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
        let finding = UnexpectedSymlinkFinding { entries: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
