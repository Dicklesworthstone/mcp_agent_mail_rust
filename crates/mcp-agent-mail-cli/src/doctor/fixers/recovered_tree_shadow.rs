//! `fm-environment_toolchain-recovered-tree-shadow` — P2.
//!
//! **Subsystem**: environment_toolchain.
//!
//! ## What's broken
//!
//! Disaster recovery / partial restores leave behind recovered copies of the
//! repo (`*_recovered_*`, e.g. `mcp_agent_mail_rust_recovered_<timestamp>`) in
//! common project roots. An agent (or operator) then cannot tell which tree is
//! live or which mailbox `am` actually resolves — the path/version-confusion
//! failure mode (br-bvq1x.10.3 / J3; observed on css and mac-mini-max where
//! recovery debris + multiple binaries + a legacy Python shadow co-existed).
//!
//! This complements the runtime-identity block now emitted by `am robot health`
//! and `am doctor check` (which names the *effective* binary/mailbox): this FM
//! proactively surfaces the recovery debris that *causes* the confusion.
//!
//! ## Detection (pure function)
//!
//! Shallow-scan each caller-supplied root's immediate children for directories
//! whose name contains `_recovered_` (the convention used by the recovery
//! tooling). Symlinks are not followed (symlink-attack defense). The detector is
//! pure w.r.t. the supplied roots so it is fully unit-testable.
//!
//! ## Fix — detect-only
//!
//! Auto-fixing is **not** offered. Which recovered copy (if any) is canonical is
//! an operator decision, and per RULE 1 the doctor never deletes — least of all
//! a whole repo tree. `fix()` is a no-op for API uniformity; the remediation
//! envelope tells the operator to confirm the canonical repo + `STORAGE_ROOT`
//! (via the `runtime_identity` block) before archiving/removing the debris.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-environment_toolchain-recovered-tree-shadow";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "environment_toolchain";

#[derive(Debug, Clone, Serialize)]
pub struct RecoveredTreeShadowFinding {
    /// The recovery-debris directory (e.g. `.../mcp_agent_mail_rust_recovered_20260516`).
    pub path: PathBuf,
    /// The scanned root the debris was found directly under.
    pub root: PathBuf,
}

impl RecoveredTreeShadowFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "recovery-debris tree {} under {} — agents may resolve the wrong repo/mailbox",
            self.path.display(),
            self.root.display()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.80,
            evidence: serde_json::json!({
                "path": self.path.to_string_lossy(),
                "root": self.root.to_string_lossy(),
                "risk": "a recovered/partial repo copy increases path/version confusion: agents cannot tell which tree is live or which storage.sqlite3 is canonical",
                "manual_step": "Confirm the canonical repo + STORAGE_ROOT (see the runtime_identity block in `am robot health` / `am doctor check`), then archive or remove the recovery-debris tree once verified",
            }),
            remediation: FindingRemediation {
                // Detect-only: no auto-fix command. Operators decide which tree
                // is canonical; the doctor never deletes a repo tree (RULE 1).
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE w.r.t. caller-supplied roots; does a shallow `read_dir` of
/// each root and flags immediate child directories that look like recovery
/// debris. Roots that cannot be read (missing, permission) are silently skipped
/// rather than producing a false positive.
pub fn detect(scan_roots: &[PathBuf]) -> Vec<RecoveredTreeShadowFinding> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for root in scan_roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Symlink-attack defense: only flag real directories, never follow
            // a symlink that merely points at one.
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if is_recovery_debris_name(name) && seen.insert(path.clone()) {
                out.push(RecoveredTreeShadowFinding {
                    path,
                    root: root.clone(),
                });
            }
        }
    }
    out
}

/// A directory name that looks like disaster-recovery debris: it contains
/// `_recovered_` (the convention the recovery tooling uses), e.g.
/// `mcp_agent_mail_rust_recovered_20260516`.
fn is_recovery_debris_name(name: &str) -> bool {
    name.contains("_recovered_")
}

/// Detect-only FM. `fix()` is a no-op (see module docs): choosing/removing a
/// recovered tree is an operator decision and RULE 1 forbids automatic deletion.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &RecoveredTreeShadowFinding,
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
    use tempfile::TempDir;

    #[test]
    fn is_recovery_debris_name_matches_recovered_trees() {
        for name in [
            "mcp_agent_mail_rust_recovered_20260516",
            "project_recovered_2026",
            "foo_recovered_bar",
        ] {
            assert!(is_recovery_debris_name(name), "should match: {name}");
        }
        for name in [
            "mcp_agent_mail_rust",
            "recovered", // no surrounding underscores
            "recovery",
            "my-recovered-tree", // dashes, not the `_recovered_` convention
            "src",
        ] {
            assert!(!is_recovery_debris_name(name), "should NOT match: {name}");
        }
    }

    #[test]
    fn detector_flags_recovered_dir_in_root() {
        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join("mcp_agent_mail_rust")).unwrap();
        fs::create_dir(td.path().join("mcp_agent_mail_rust_recovered_20260516")).unwrap();
        // A non-directory with the magic substring must NOT be flagged.
        fs::write(td.path().join("notes_recovered_x.txt"), b"x").unwrap();

        let findings = detect(&[td.path().to_path_buf()]);
        assert_eq!(findings.len(), 1, "exactly the recovered dir: {findings:?}");
        assert!(
            findings[0]
                .path
                .to_string_lossy()
                .contains("mcp_agent_mail_rust_recovered_20260516")
        );
    }

    #[test]
    fn detector_empty_when_no_debris() {
        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join("src")).unwrap();
        fs::create_dir(td.path().join("crates")).unwrap();
        assert!(detect(&[td.path().to_path_buf()]).is_empty());
    }

    #[test]
    fn detector_skips_missing_root_without_panicking() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("does-not-exist");
        assert!(detect(&[missing]).is_empty());
    }

    #[test]
    fn detector_dedupes_same_path_across_roots() {
        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join("x_recovered_1")).unwrap();
        // Pass the same root twice — the finding must not be duplicated.
        let findings = detect(&[td.path().to_path_buf(), td.path().to_path_buf()]);
        assert_eq!(findings.len(), 1, "deduped across roots: {findings:?}");
    }

    #[test]
    fn finding_is_p2_detect_only() {
        let f = RecoveredTreeShadowFinding {
            path: PathBuf::from("/data/projects/mcp_agent_mail_rust_recovered_20260516"),
            root: PathBuf::from("/data/projects"),
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "environment_toolchain");
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("recovered_20260516"));
        assert!(s.contains("runtime_identity"));
    }
}
