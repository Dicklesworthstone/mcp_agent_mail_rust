//! `fm-archive-state-files-unexpected-symlink-in-archive` — P1
//! auto-fix via symlink-aware `Op::Rename` (quarantine).
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
//! **Auto-fix via symlink-aware `Op::Rename` (quarantine).** Each
//! unexpected symlink is MOVED — never dereferenced — into
//! `<run-dir>/quarantine/archive-symlinks/<basename>.<ns>` through
//! the chokepoint. This immediately removes the symlink from the
//! live archive serving path (neutralizing any exfil vector — e.g.
//! a link aliasing `<storage>/.../foo.md` at `/etc/shadow` that
//! `am robot thread` would otherwise read through), while
//! PRESERVING the link itself in quarantine for forensics. The
//! chokepoint hash-witnesses the link by its target STRING (it is
//! never followed), so `am doctor undo <run-id>` renames the
//! symlink back byte-identically if the operator decides it was
//! legitimate.
//!
//! Per AGENTS.md RULE 1 this is NEVER a delete: the symlink tree
//! entry is preserved under quarantine. Vanished entries (the
//! symlink was already removed) count as `actions_skipped`
//! (idempotent).
//!
//! Quarantining-then-undo is strictly safer than leaving a live
//! symlink in the archive pending manual operator action: the
//! exfil vector is gone immediately, the evidence is preserved,
//! and the move is fully reversible.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
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
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` quarantines all {} unexpected symlink(s) via symlink-aware Op::Rename into `<run-dir>/quarantine/archive-symlinks/` — the link is MOVED (never dereferenced), removing the exfil vector from the live archive while preserving the link for forensics. Reversible via `am doctor undo <run-id>`.",
                    self.entries.len(),
                ),
                "manual_remediation": {
                    "steps": [
                        "Auto-fix (preferred): `am doctor fix --only fm-archive-state-files-unexpected-symlink-in-archive --yes`. Quarantines each symlink (moved, not dereferenced) into `<run-dir>/quarantine/archive-symlinks/`; reversible via `am doctor undo <run-id>`.",
                        "Before/after auto-fixing, INVESTIGATE each target: `ls -la <path>` to inspect the symlink + target. A target outside `<storage_root>` (e.g. `/etc/shadow`) is a strong tampering signal.",
                        "If the symlink was a legitimate alias the archive needs as content: `am doctor undo <run-id>` to restore it, then `cp <target> <path>` to recreate the archive entry as a regular file.",
                        "Re-run `am doctor fix --only fm-archive-state-files-unexpected-symlink-in-archive --list` to confirm the archive is clean.",
                    ],
                    "warning": "Symlinks in the archive can be a SECURITY signal (attacker aliasing archive files at sensitive system paths). The auto-fix removes the live exfil vector immediately and preserves the link in quarantine — investigate the quarantined target before deciding it was legitimate.",
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor fix --only {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: !self.entries.is_empty(),
                estimated_actions: self.entries.len(),
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

/// Fixer. Quarantines each unexpected symlink via symlink-aware
/// `Op::Rename` (the chokepoint moves the link, never follows it).
/// Vanished entries count as `actions_skipped` (idempotent).
pub fn fix(
    ctx: &MutateContext,
    finding: &UnexpectedSymlinkFinding,
) -> Result<FixOutcome, MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    let mut quarantined_paths = Vec::new();
    // One nanosecond base + per-entry index disambiguates
    // same-basename symlinks at the quarantine destination.
    let base_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (idx, entry) in finding.entries.iter().enumerate() {
        // `symlink_metadata` does not follow the link — we only
        // check that the symlink still exists, never read its target.
        if std::fs::symlink_metadata(&entry.path).is_err() {
            actions_skipped += 1;
            continue;
        }
        let basename = entry
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "symlink".to_string());
        let dest = ctx
            .run_dir
            .join("quarantine")
            .join("archive-symlinks")
            .join(format!("{basename}.{}", base_ns + idx as u128));
        mutate(ctx, &entry.path, Op::Rename { to: dest.clone() })?;
        actions_taken += 1;
        quarantined_paths.push(dest);
    }
    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
        quarantined_paths,
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
        assert!(s.contains("\"auto_fixable\":true"));
        assert!(s.contains("\"estimated_actions\":1"));
        assert!(s.contains("auto_fix_summary"));
        // Target path appears in evidence (for operator visibility).
        assert!(s.contains("shadow"));
    }

    fn ctx_for(td: &tempfile::TempDir, run_id: &str) -> crate::doctor::mutate::MutateContext {
        use std::fs;
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        crate::doctor::mutate::MutateContext {
            run_id: run_id.into(),
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
        }
    }

    /// **NEGATIVE TEST FIRST**: empty finding → no-op (no actions,
    /// no skips).
    #[test]
    fn fixer_with_no_entries_is_a_no_op() {
        let td = tempfile::TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__sym_empty");
        let finding = UnexpectedSymlinkFinding { entries: vec![] };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 0);
    }

    /// **NEGATIVE**: a symlink that vanished between detect and fix
    /// is skipped, never errors.
    #[cfg(unix)]
    #[test]
    fn fixer_skips_vanished_symlink() {
        let td = tempfile::TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__sym_vanished");
        let finding = UnexpectedSymlinkFinding {
            entries: vec![UnexpectedSymlinkEntry {
                path: td.path().join("ghost-link.md"),
                target: Some("/some/target".into()),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// Positive: an unexpected symlink (pointing at a sensitive
    /// target outside the archive) is quarantined — MOVED, never
    /// dereferenced. The target's bytes are never read.
    #[cfg(unix)]
    #[test]
    fn fixer_quarantines_symlink_without_dereferencing() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let td = tempfile::TempDir::new().unwrap();
        // A "sensitive" target outside any archive path.
        let secret = td.path().join("secret.txt");
        fs::write(&secret, b"top-secret").unwrap();
        // The archive symlink aliasing it.
        let archive_dir = td.path().join("projects").join("demo");
        fs::create_dir_all(&archive_dir).unwrap();
        let link = archive_dir.join("aliased.md");
        symlink(&secret, &link).unwrap();

        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__sym_fix");
        let finding = UnexpectedSymlinkFinding {
            entries: vec![UnexpectedSymlinkEntry {
                path: link.clone(),
                target: Some(secret.clone()),
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);
        assert_eq!(outcome.quarantined_paths.len(), 1);

        // Live archive link gone (exfil vector removed).
        assert!(fs::symlink_metadata(&link).is_err());
        // Quarantined as a symlink, target preserved, NOT a copy.
        let q = &outcome.quarantined_paths[0];
        let q_meta = fs::symlink_metadata(q).unwrap();
        assert!(q_meta.file_type().is_symlink());
        assert_eq!(fs::read_link(q).unwrap(), secret);
        // Secret target never touched.
        assert_eq!(fs::read(&secret).unwrap(), b"top-secret");
    }

    /// Round-trip: quarantine → undo → the symlink reappears at its
    /// original archive path, byte-identical (same target).
    #[cfg(unix)]
    #[test]
    fn round_trip_quarantine_then_undo_restores_symlink() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let td = tempfile::TempDir::new().unwrap();
        let target = td.path().join("target.bin");
        fs::write(&target, b"x").unwrap();
        let link = td.path().join("projects").join("demo").join("aliased.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();
        let original_target = fs::read_link(&link).unwrap();

        let run_id = "2026-05-20T00-00-00Z__sym_rt";
        let ctx = ctx_for(&td, run_id);
        let finding = UnexpectedSymlinkFinding {
            entries: vec![UnexpectedSymlinkEntry {
                path: link.clone(),
                target: Some(target.clone()),
            }],
        };
        assert_eq!(fix(&ctx, &finding).expect("fix").actions_taken, 1);
        assert!(fs::symlink_metadata(&link).is_err(), "link quarantined");

        drop(ctx);
        let summary = crate::doctor::undo::run_undo_with_scopes(
            td.path(),
            run_id,
            false,
            true,
            &[td.path().to_path_buf()],
        )
        .expect("run_undo");
        assert!(summary.failures.is_empty(), "undo failures: {:?}", summary.failures);

        // Symlink restored at original path, same target.
        let restored = fs::symlink_metadata(&link).unwrap();
        assert!(restored.file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), original_target);
    }
}
