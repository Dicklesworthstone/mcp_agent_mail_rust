//! `fm-archive-state-files-archive-message-artifact-anomalies` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Each persisted message produces two archive artifacts:
//!
//! - The **canonical** message file at
//!   `<storage_root>/projects/<slug>/messages/YYYY/MM/<id>.md`
//!   — the source of truth for body + metadata.
//! - A per-mailbox **copy** at
//!   `<storage_root>/projects/<slug>/agents/<name>/{inbox,outbox}/...`
//!   — convenience-index for fast per-agent listing.
//!
//! Both are written through the same storage-layer commit
//! coalescer, but a half-flushed commit, partial archive restore,
//! or DB-only operation can leave the DB pointing at artifacts
//! that don't exist on disk. Two distinct anomalies fall out:
//!
//! - **MissingCanonicalMessage** — a row exists in `messages` but
//!   no canonical `<id>.md` is present. The message body cannot be
//!   read by `am robot message`, `resource://message/<id>`, or any
//!   FTS V3 reindex. **Data loss signal**.
//! - **MessageRecipientCopyMismatch** — a row in
//!   `message_recipients` references an agent's inbox/outbox copy
//!   that is missing, or the on-disk copy disagrees with the
//!   canonical (digest divergence). The per-agent listing is
//!   silently wrong; `fetch_inbox` may omit messages the agent
//!   was actually sent.
//!
//! These two variants share a remediation playbook (rebuild the
//! affected artifact from the surviving side), so the FM bundles
//! them. Mirrors FM13 (`archive_db_drift_anomalies`).
//!
//! ## Detection
//!
//! Filters an `ArchiveAnomalyReport` for
//! `ArchiveAnomalyKind::MissingCanonicalMessage` and
//! `ArchiveAnomalyKind::MessageRecipientCopyMismatch`. In the normal
//! doctor dispatcher this report is produced by
//! `scan_archive_anomalies_with_db(storage_root, db_path)` so the DB-aware
//! variants can actually be observed. Direct detector calls with only a
//! storage root fall back to archive-only scanning and therefore won't emit
//! this FM.
//!
//! ## Fix
//!
//! **Detect-only.** Restoring the missing artifact requires
//! deciding which side is authoritative:
//!
//! 1. Run `am doctor archive-verify --json` and preserve the current
//!    DB/archive before mutating either side.
//! 2. If the archive is authoritative and DB rows/copies are stale:
//!    `am doctor reconstruct --dry-run --json` previews the DB rebuild;
//!    `am doctor reconstruct --yes` applies it after confirmation.
//! 3. If the DB is authoritative and the archive is partial: restore the
//!    missing canonical/mailbox artifacts from a known-good archive backup
//!    or rebuild them manually from preserved DB evidence. There is no
//!    broad DB-to-archive rewrite command.
//! 4. Re-run this detector to confirm zero residual mismatches.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-archive-message-artifact-anomalies";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct MissingCanonicalEntry {
    pub project_slug: String,
    pub message_id: i64,
    pub db_subject: String,
    pub db_sender: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipientCopyMismatchEntry {
    pub project_slug: String,
    pub message_id: i64,
    pub agent_name: String,
    pub mailbox: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveMessageArtifactFinding {
    pub missing_canonical: Vec<MissingCanonicalEntry>,
    pub recipient_copy_mismatches: Vec<RecipientCopyMismatchEntry>,
}

impl ArchiveMessageArtifactFinding {
    pub fn total_entries(&self) -> usize {
        self.missing_canonical.len() + self.recipient_copy_mismatches.len()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} archive-vs-DB message-artifact mismatch(es): {} missing canonical message file(s), {} per-agent mailbox copy mismatch(es)",
            self.total_entries(),
            self.missing_canonical.len(),
            self.recipient_copy_mismatches.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "missing_canonical": self.missing_canonical,
                "recipient_copy_mismatches": self.recipient_copy_mismatches,
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor archive-verify --json` and preserve the current DB/archive before mutating either side.",
                        "For each `<project_slug, message_id>`: compare DB evidence with the canonical archive file and mailbox copies. Pick the authoritative side.",
                        "If archive is authoritative: run `am doctor reconstruct --dry-run --json` to preview the DB rebuild, then `am doctor reconstruct --yes` after preserving forensics/backups.",
                        "If DB is authoritative: restore missing canonical/mailbox artifacts from a known-good archive backup or rebuild them manually from preserved DB evidence; there is no broad DB-to-archive rewrite command.",
                        "`am doctor archive-normalize --dry-run` is only for safe archive hygiene such as project metadata and duplicate canonical files; it does not regenerate mailbox copies from DB rows.",
                        "Re-run `am doctor fix --only fm-archive-state-files-archive-message-artifact-anomalies --list` to confirm zero residual mismatches.",
                    ],
                    "warning": "Auto-fix is intentionally NOT implemented — picking the wrong authoritative side overwrites good content with bad. Inspect both sides before reconstructing.",
                    "data_loss_signal": "MissingCanonicalMessage is a data-loss signal: the message body can no longer be read by `am robot message`, FTS V3, or any resource:// view. Resolve P1.",
                    "common_causes": [
                        "Half-flushed commit coalescer write — canonical landed but mailbox copy didn't, or vice versa.",
                        "Partial archive restore from backup older than the SQLite index.",
                        "Manual filesystem deletion (e.g., `rm` on an archive subtree) without rebuilding the DB.",
                        "Rebase / branch reset on the per-project git archive that dropped commits the DB still references.",
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

pub fn detect(inputs: &DetectInputs) -> Vec<ArchiveMessageArtifactFinding> {
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
    let mut missing_canonical: Vec<MissingCanonicalEntry> = Vec::new();
    let mut recipient_copy_mismatches: Vec<RecipientCopyMismatchEntry> = Vec::new();
    for a in &report.anomalies {
        match &a.kind {
            ArchiveAnomalyKind::MissingCanonicalMessage {
                project_slug,
                message_id,
                db_subject,
                db_sender,
            } => {
                missing_canonical.push(MissingCanonicalEntry {
                    project_slug: project_slug.clone(),
                    message_id: *message_id,
                    db_subject: db_subject.clone(),
                    db_sender: db_sender.clone(),
                });
            }
            ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                project_slug,
                message_id,
                agent_name,
                mailbox,
                detail,
            } => {
                recipient_copy_mismatches.push(RecipientCopyMismatchEntry {
                    project_slug: project_slug.clone(),
                    message_id: *message_id,
                    agent_name: agent_name.clone(),
                    mailbox: mailbox.as_str().to_string(),
                    detail: detail.clone(),
                });
            }
            _ => {}
        }
    }
    if missing_canonical.is_empty() && recipient_copy_mismatches.is_empty() {
        return Vec::new();
    }
    vec![ArchiveMessageArtifactFinding {
        missing_canonical,
        recipient_copy_mismatches,
    }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ArchiveMessageArtifactFinding,
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
    use mcp_agent_mail_db::archive_anomaly::{ArchiveAnomaly, MailboxCopyKind};

    /// **NEGATIVE TEST FIRST**: empty report → no finding.
    #[test]
    fn detector_skips_clean_report() {
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(ArchiveAnomalyReport::new()),
        };
        assert!(detect(&inputs).is_empty());
    }

    /// **NEGATIVE**: unrelated variants → no finding. Specifically
    /// pins that we don't accidentally pick up FM7
    /// (UnexpectedSymlink), FM13 (ArchiveDbProjectMismatch /
    /// ArchiveDbCountDrift), or FM11 (OrphanedAgentProfile).
    #[test]
    fn detector_skips_report_with_only_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report
            .anomalies
            .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                path: "/x/link.md".into(),
                target: Some("/etc/passwd".into()),
            }));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbCountDrift {
                archive_count: 100,
                db_count: 90,
                drift: 10,
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug: "x".to_string(),
                archive_human_key: None,
                detail: "d".to_string(),
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
    fn detector_flags_single_missing_canonical() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingCanonicalMessage {
                project_slug: "demo".to_string(),
                message_id: 42,
                db_subject: "Test subject".to_string(),
                db_sender: "AlphaWaterfall".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].missing_canonical.len(), 1);
        assert!(findings[0].recipient_copy_mismatches.is_empty());
        assert_eq!(findings[0].missing_canonical[0].message_id, 42);
        assert_eq!(findings[0].missing_canonical[0].project_slug, "demo");
    }

    #[test]
    fn detector_flags_inbox_copy_mismatch() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                project_slug: "demo".to_string(),
                message_id: 7,
                agent_name: "BravoMountain".to_string(),
                mailbox: MailboxCopyKind::Inbox,
                detail: "inbox copy missing".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].recipient_copy_mismatches.len(), 1);
        let entry = &findings[0].recipient_copy_mismatches[0];
        assert_eq!(entry.mailbox, "inbox");
        assert_eq!(entry.agent_name, "BravoMountain");
        assert_eq!(entry.message_id, 7);
    }

    #[test]
    fn detector_flags_outbox_copy_mismatch_with_distinct_mailbox_string() {
        // Pin that `mailbox` serializes to the right string per
        // MailboxCopyKind. A regression that swapped Inbox/Outbox
        // would silently report wrong mailbox sides to operators.
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                project_slug: "demo".to_string(),
                message_id: 99,
                agent_name: "CharlieRiver".to_string(),
                mailbox: MailboxCopyKind::Outbox,
                detail: "outbox copy digest differs".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings[0].recipient_copy_mismatches[0].mailbox, "outbox");
    }

    #[test]
    fn detector_bundles_both_kinds_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingCanonicalMessage {
                project_slug: "p".to_string(),
                message_id: 1,
                db_subject: "s".to_string(),
                db_sender: "AlphaWaterfall".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                project_slug: "p".to_string(),
                message_id: 2,
                agent_name: "BravoMountain".to_string(),
                mailbox: MailboxCopyKind::Inbox,
                detail: "missing".to_string(),
            },
        ));
        // Unrelated variant must be silently dropped.
        report
            .anomalies
            .push(ArchiveAnomaly::now(ArchiveAnomalyKind::UnexpectedSymlink {
                path: "/x".into(),
                target: None,
            }));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].missing_canonical.len(), 1);
        assert_eq!(findings[0].recipient_copy_mismatches.len(), 1);
        assert_eq!(findings[0].total_entries(), 2);
    }

    #[test]
    fn detector_aggregates_multiple_of_each_kind() {
        let mut report = ArchiveAnomalyReport::new();
        for i in 0..4 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::MissingCanonicalMessage {
                    project_slug: "p".to_string(),
                    message_id: i,
                    db_subject: format!("s{i}"),
                    db_sender: "AlphaWaterfall".to_string(),
                },
            ));
        }
        for i in 0..2 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::MessageRecipientCopyMismatch {
                    project_slug: "p".to_string(),
                    message_id: 100 + i,
                    agent_name: "BravoMountain".to_string(),
                    mailbox: if i == 0 {
                        MailboxCopyKind::Inbox
                    } else {
                        MailboxCopyKind::Outbox
                    },
                    detail: format!("d{i}"),
                },
            ));
        }
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].missing_canonical.len(), 4);
        assert_eq!(findings[0].recipient_copy_mismatches.len(), 2);
        assert_eq!(findings[0].total_entries(), 6);
    }

    #[test]
    fn finding_serializes_with_data_loss_signal_and_remediation() {
        let f = ArchiveMessageArtifactFinding {
            missing_canonical: vec![MissingCanonicalEntry {
                project_slug: "p".to_string(),
                message_id: 1,
                db_subject: "x".to_string(),
                db_sender: "AlphaWaterfall".to_string(),
            }],
            recipient_copy_mismatches: vec![RecipientCopyMismatchEntry {
                project_slug: "p".to_string(),
                message_id: 2,
                agent_name: "BravoMountain".to_string(),
                mailbox: "inbox".to_string(),
                detail: "missing".to_string(),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("missing_canonical"));
        assert!(s.contains("recipient_copy_mismatches"));
        assert!(s.contains("data_loss_signal"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am doctor reconstruct"));
        assert!(s.contains("am doctor archive-verify --json"));
        assert!(s.contains("am doctor archive-normalize --dry-run"));
        assert!(!s.contains("--canonical-from-db"));
        assert!(!s.contains("--reindex"));
        assert!(!s.contains("--rebuild-mailbox-copies"));
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
        let finding = ArchiveMessageArtifactFinding {
            missing_canonical: vec![],
            recipient_copy_mismatches: vec![],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
