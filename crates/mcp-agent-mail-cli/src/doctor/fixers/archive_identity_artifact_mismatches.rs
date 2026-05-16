//! `fm-archive-state-files-archive-identity-artifact-mismatches` — P1
//! detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Two distinct DB rows in the SQLite index can reference archive
//! artifacts that have drifted from their on-disk reality:
//!
//! - **AgentProfileMismatch** — an `agents` row references an
//!   archive profile path
//!   (`<storage_root>/projects/<slug>/agents/<name>/profile.json`)
//!   that is missing on disk, OR the on-disk `profile.json`
//!   disagrees with the DB identity fields (name, program, model,
//!   capabilities). `whois`, `register_agent` validation, and
//!   `am robot agents` rely on this artifact for human-auditable
//!   identity provenance.
//! - **FileReservationArtifactMismatch** — a `file_reservations`
//!   row references a stable-id artifact in the archive
//!   (`<storage_root>/projects/<slug>/file_reservations/id-<id>.json`)
//!   that is missing OR disagrees with the DB row's stable id,
//!   holder agent, or path pattern. The pre-commit guard uses the on-disk
//!   artifact to decide whether to block a commit, so a stale
//!   artifact can either over-block (false-positive guard fire)
//!   or under-block (silent reservation bypass).
//!
//! Both variants are DB-vs-archive cross-checks and share a
//! remediation playbook (preserve both sides, decide which is
//! authoritative, then reconcile via reconstruct/restore). They
//! are bundled into ONE FM mirroring FM13
//! (`archive_db_drift_anomalies`) and FM14
//! (`archive_message_artifact_anomalies`).
//!
//! This FM completes coverage of the 19 `ArchiveAnomalyKind`
//! variants across the doctor's FM surface.
//!
//! ## Detection
//!
//! Filters an `ArchiveAnomalyReport` for
//! `ArchiveAnomalyKind::AgentProfileMismatch` and
//! `ArchiveAnomalyKind::FileReservationArtifactMismatch`. In the
//! normal doctor dispatcher this report is produced by
//! `scan_archive_anomalies_with_db(storage_root, db_path)` so the
//! DB-aware variants can actually be observed. Direct detector
//! calls with only a storage root fall back to archive-only
//! scanning and therefore won't emit this FM.
//!
//! ## Fix
//!
//! **Detect-only.** Reconciling DB-vs-archive identity / lease
//! drift requires operator judgment:
//!
//! 1. Run `am doctor archive-verify --json` and preserve the
//!    current DB/archive before mutating either side.
//! 2. For each affected entry: compare the on-disk artifact (if
//!    present) with the DB row. Pick the authoritative side.
//! 3. If archive is authoritative: `am doctor reconstruct --yes`
//!    rebuilds the SQLite rows from the archive artifacts.
//! 4. If DB is authoritative: restore the missing archive
//!    artifacts from a known-good backup, OR manually regenerate
//!    them by re-running `register_agent` / re-issuing the file
//!    reservation. There is no broad DB-to-archive rewrite
//!    command.
//! 5. Re-run this detector to confirm zero residual mismatches.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use mcp_agent_mail_db::archive_anomaly::{
    ArchiveAnomalyKind, ArchiveAnomalyReport, scan_archive_anomalies,
};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-archive-identity-artifact-mismatches";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct AgentProfileMismatchEntry {
    pub project_slug: String,
    pub agent_name: String,
    pub profile_path: PathBuf,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReservationArtifactMismatchEntry {
    pub project_slug: String,
    pub reservation_id: i64,
    pub artifact_path: PathBuf,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveIdentityArtifactFinding {
    pub agent_profile_mismatches: Vec<AgentProfileMismatchEntry>,
    pub reservation_artifact_mismatches: Vec<ReservationArtifactMismatchEntry>,
}

impl ArchiveIdentityArtifactFinding {
    pub fn total_entries(&self) -> usize {
        self.agent_profile_mismatches.len() + self.reservation_artifact_mismatches.len()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} archive-vs-DB identity/lease artifact mismatch(es): {} agent profile mismatch(es), {} file reservation artifact mismatch(es)",
            self.total_entries(),
            self.agent_profile_mismatches.len(),
            self.reservation_artifact_mismatches.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "agent_profile_mismatches": self.agent_profile_mismatches,
                "reservation_artifact_mismatches": self.reservation_artifact_mismatches,
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor archive-verify --json` and preserve the current DB/archive before mutating either side.",
                        "For each entry: compare the on-disk artifact (if present) with the DB row. Pick the authoritative side.",
                        "If archive is authoritative: `am doctor reconstruct --dry-run --json` previews the SQLite rebuild; `am doctor reconstruct --yes` applies it after preserving forensics/backups.",
                        "If DB is authoritative for an agent profile: restore the profile from a backup, OR re-run `register_agent` to write a fresh profile from current DB state.",
                        "If DB is authoritative for a file reservation: restore the artifact from a backup, OR re-issue the reservation (`file_reservation_paths` MCP tool) to regenerate the artifact.",
                        "Re-run `am doctor fix --only fm-archive-state-files-archive-identity-artifact-mismatches --list` to confirm zero residual mismatches.",
                    ],
                    "warning": "Auto-fix is intentionally NOT implemented — picking the wrong authoritative side either over-blocks pre-commit (false positive) or under-blocks (silent reservation bypass). Inspect both sides before reconstructing.",
                    "pre_commit_guard_impact": "FileReservationArtifactMismatch directly affects the pre-commit guard. A drifted artifact can either reject legitimate commits (over-block) or allow conflicting writes that should have been rejected (under-block). Treat as P1.",
                    "common_causes": [
                        "Half-flushed commit coalescer write — DB row landed but the JSON artifact didn't, or vice versa.",
                        "Partial archive restore from a backup older than the SQLite index.",
                        "Manual filesystem edit of `profile.json` / `<id>.json` without an accompanying DB update.",
                        "TTL renewal that bumped the DB row but didn't rewrite the artifact (or vice versa).",
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

pub fn detect(inputs: &DetectInputs) -> Vec<ArchiveIdentityArtifactFinding> {
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
    let mut agent_profile_mismatches: Vec<AgentProfileMismatchEntry> = Vec::new();
    let mut reservation_artifact_mismatches: Vec<ReservationArtifactMismatchEntry> = Vec::new();
    for a in &report.anomalies {
        match &a.kind {
            ArchiveAnomalyKind::AgentProfileMismatch {
                project_slug,
                agent_name,
                profile_path,
                detail,
            } => {
                agent_profile_mismatches.push(AgentProfileMismatchEntry {
                    project_slug: project_slug.clone(),
                    agent_name: agent_name.clone(),
                    profile_path: profile_path.clone(),
                    detail: detail.clone(),
                });
            }
            ArchiveAnomalyKind::FileReservationArtifactMismatch {
                project_slug,
                reservation_id,
                artifact_path,
                detail,
            } => {
                reservation_artifact_mismatches.push(ReservationArtifactMismatchEntry {
                    project_slug: project_slug.clone(),
                    reservation_id: *reservation_id,
                    artifact_path: artifact_path.clone(),
                    detail: detail.clone(),
                });
            }
            _ => {}
        }
    }
    if agent_profile_mismatches.is_empty() && reservation_artifact_mismatches.is_empty() {
        return Vec::new();
    }
    vec![ArchiveIdentityArtifactFinding {
        agent_profile_mismatches,
        reservation_artifact_mismatches,
    }]
}

pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &ArchiveIdentityArtifactFinding,
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

    /// **NEGATIVE**: only unrelated variants → no finding. Pins
    /// that we don't accidentally pick up FM11
    /// (OrphanedAgentProfile / MalformedAgentProfile — those are
    /// archive-only, not DB-vs-archive). Also pins distinction
    /// from FM13 (ArchiveDbProjectMismatch) and FM14
    /// (MissingCanonicalMessage / MessageRecipientCopyMismatch).
    #[test]
    fn detector_skips_report_with_only_unrelated_anomalies() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::OrphanedAgentProfile {
                profile_path: "/x/y/agents/AlphaWaterfall/profile.json".into(),
                agent_name: "AlphaWaterfall".to_string(),
                parent_project_dir: "/x/y".into(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::ArchiveDbProjectMismatch {
                archive_slug: "p".to_string(),
                archive_human_key: None,
                detail: "d".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::MissingCanonicalMessage {
                project_slug: "p".to_string(),
                message_id: 1,
                db_subject: "s".to_string(),
                db_sender: "BravoMountain".to_string(),
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
    fn detector_flags_single_agent_profile_mismatch() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::AgentProfileMismatch {
                project_slug: "demo".to_string(),
                agent_name: "AlphaWaterfall".to_string(),
                profile_path: "/x/projects/demo/agents/AlphaWaterfall/profile.json".into(),
                detail: "DB program=claude-code, archive program=cursor".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].agent_profile_mismatches.len(), 1);
        assert!(findings[0].reservation_artifact_mismatches.is_empty());
        let entry = &findings[0].agent_profile_mismatches[0];
        assert_eq!(entry.agent_name, "AlphaWaterfall");
        assert_eq!(entry.project_slug, "demo");
    }

    #[test]
    fn detector_flags_single_reservation_artifact_mismatch() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::FileReservationArtifactMismatch {
                project_slug: "demo".to_string(),
                reservation_id: 42,
                artifact_path: "/x/projects/demo/file_reservations/42.json".into(),
                detail: "DB ttl=3600, archive ttl=7200".to_string(),
            },
        ));
        let inputs = DetectInputs {
            storage_root_override: None,
            report_override: Some(report),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reservation_artifact_mismatches.len(), 1);
        assert!(findings[0].agent_profile_mismatches.is_empty());
        let entry = &findings[0].reservation_artifact_mismatches[0];
        assert_eq!(entry.reservation_id, 42);
        assert_eq!(entry.project_slug, "demo");
    }

    #[test]
    fn detector_bundles_both_kinds_into_one_finding() {
        let mut report = ArchiveAnomalyReport::new();
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::AgentProfileMismatch {
                project_slug: "p".to_string(),
                agent_name: "AlphaWaterfall".to_string(),
                profile_path: "/x/agents/AlphaWaterfall/profile.json".into(),
                detail: "model mismatch".to_string(),
            },
        ));
        report.anomalies.push(ArchiveAnomaly::now(
            ArchiveAnomalyKind::FileReservationArtifactMismatch {
                project_slug: "p".to_string(),
                reservation_id: 1,
                artifact_path: "/x/file_reservations/1.json".into(),
                detail: "paths mismatch".to_string(),
            },
        ));
        // Unrelated variant: must be silently dropped.
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
        assert_eq!(findings[0].agent_profile_mismatches.len(), 1);
        assert_eq!(findings[0].reservation_artifact_mismatches.len(), 1);
        assert_eq!(findings[0].total_entries(), 2);
    }

    #[test]
    fn detector_aggregates_multiple_of_each_kind() {
        let mut report = ArchiveAnomalyReport::new();
        for i in 0..3 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::AgentProfileMismatch {
                    project_slug: "p".to_string(),
                    agent_name: format!("Agent{i}"),
                    profile_path: format!("/x/agents/Agent{i}/profile.json").into(),
                    detail: format!("d{i}"),
                },
            ));
        }
        for i in 0..2 {
            report.anomalies.push(ArchiveAnomaly::now(
                ArchiveAnomalyKind::FileReservationArtifactMismatch {
                    project_slug: "p".to_string(),
                    reservation_id: 100 + i,
                    artifact_path: format!("/x/file_reservations/{}.json", 100 + i).into(),
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
        assert_eq!(findings[0].agent_profile_mismatches.len(), 3);
        assert_eq!(findings[0].reservation_artifact_mismatches.len(), 2);
        assert_eq!(findings[0].total_entries(), 5);
    }

    #[test]
    fn finding_serializes_with_pre_commit_guard_callout() {
        let f = ArchiveIdentityArtifactFinding {
            agent_profile_mismatches: vec![AgentProfileMismatchEntry {
                project_slug: "p".to_string(),
                agent_name: "AlphaWaterfall".to_string(),
                profile_path: "/x/profile.json".into(),
                detail: "x".to_string(),
            }],
            reservation_artifact_mismatches: vec![ReservationArtifactMismatchEntry {
                project_slug: "p".to_string(),
                reservation_id: 1,
                artifact_path: "/x/1.json".into(),
                detail: "y".to_string(),
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("agent_profile_mismatches"));
        assert!(s.contains("reservation_artifact_mismatches"));
        assert!(s.contains("pre_commit_guard_impact"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am doctor reconstruct"));
        assert!(s.contains("am doctor archive-verify"));
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
        let finding = ArchiveIdentityArtifactFinding {
            agent_profile_mismatches: vec![],
            reservation_artifact_mismatches: vec![],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
