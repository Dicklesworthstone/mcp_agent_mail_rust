//! `fm-share_export_state-half-finished-bundle-after-crash` — P1
//! detect-only.
//!
//! **Subsystem**: share_export_state.
//!
//! ## What's broken
//!
//! The share-export pipeline writes bundles to
//! `<project_root>/archived_mailbox_states/` or a caller-provided
//! share-output directory via a staged publish sequence
//! (`.<bundle>.bundle-stage.*` plus `.<bundle>.bundle-backup.*`).
//! When the process
//! crashes mid-pipeline (kernel reboot, OOM kill, agent-mail
//! server killed by signal), one of these debris patterns can
//! remain:
//!
//! - **Stale temp dirs** matching the current staged-output
//!   names (`.<bundle>.bundle-stage.*`,
//!   `.<bundle>.bundle-backup.*`, `.bundle-zip.*`) or the legacy
//!   `am-share-*` names. They must have been sitting on disk for
//!   more than the stale threshold (default 60 min). Live
//!   pipelines never leave these around — they're cleaned up at
//!   the end of each stage.
//! - **Partial bundles**: a published bundle dir whose
//!   `manifest.json` exists but the required `mailbox.sqlite3`
//!   payload does not, OR `bundle.zip.partial` exists without
//!   `bundle.zip` (legacy zip-writer flush crashed mid-write).
//!   `manifest.sig.json` is intentionally optional: the current
//!   share pipeline writes it only when `--signing-key` is used,
//!   so an unsigned bundle is not a partial bundle.
//!
//! Either category indicates the share-export was aborted; the
//! debris bloats disk and confuses operators inspecting the
//! bundle directory. P1 because a partial bundle is exposed
//! through the share-export API as if it were valid.
//!
//! ## Detection (pure)
//!
//! Walks `<project_root>/archived_mailbox_states/` and:
//!
//! 1. Records any subdir (max depth 4) whose name starts with
//!    or matches one of the temp patterns AND whose mtime is older than
//!    `stale_threshold_secs` (default 3600 = 60 min).
//! 2. Records any immediate-subdir bundle that has
//!    `manifest.json` but no `mailbox.sqlite3`.
//! 3. Records any immediate-subdir bundle that has
//!    `bundle.zip.partial` but no `bundle.zip`.
//!
//! Emits one aggregated finding when at least one debris entry
//! is found. The mtime check ensures we don't false-flag a
//! live, in-progress export — only debris that's been sitting.
//!
//! ## Fix
//!
//! **Detect-only.** The repair spec calls for Op::Rename of
//! every debris entry into `<run-dir>/quarantine/share-debris/`.
//! Multi-step with operator-visible side effects; deferred.
//! Manual remediation: move each entry into a private archive
//! after inspecting — debris contains in-progress export bytes
//! and may include user data that the operator wants to
//! preserve for forensics.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const FM_ID: &str = "fm-share_export_state-half-finished-bundle-after-crash";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "share_export_state";

/// Project-local archive directory used by `am archive save` and
/// by operators who place share-output bundles next to saved
/// mailbox states.
const ARCHIVE_STATES_DIR: &str = "archived_mailbox_states";

/// Default age above which a temp dir is considered stale.
/// Live exports churn through temp dirs in seconds; 60 min is
/// well above any real flush window.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 3600;

/// Maximum walk depth from `<project_root>/archived_mailbox_states/`.
/// Prevents pathological recursion if a bundle dir nests
/// pipeline-related dirs more than expected.
const MAX_WALK_DEPTH: u32 = 4;

/// ORDER MATTERS: longest-prefix-first so legacy
/// `am-share-finalize-rollback-xyz` is attributed to the rollback
/// family rather than the broader finalize family.
const LEGACY_TEMP_PREFIXES: &[&str] = &[
    "am-share-finalize-rollback-",
    "am-share-finalize-",
    "am-share-stage-",
    "am-share-backup-",
];

#[derive(Debug, Clone, Serialize)]
pub struct StaleTempDirEntry {
    pub path: PathBuf,
    pub age_secs: u64,
    pub matched_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialBundleReason {
    /// `manifest.json` exists but the required `mailbox.sqlite3`
    /// payload is absent.
    MissingDatabase,
    /// `bundle.zip.partial` exists without `bundle.zip` — zip
    /// writer crashed mid-flush.
    PartialZip,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartialBundleEntry {
    pub bundle_dir: PathBuf,
    pub reason: PartialBundleReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareHalfFinishedBundleFinding {
    pub archive_root: PathBuf,
    pub stale_threshold_secs: u64,
    pub stale_temp_dirs: Vec<StaleTempDirEntry>,
    pub partial_bundles: Vec<PartialBundleEntry>,
}

impl ShareHalfFinishedBundleFinding {
    pub fn total_entries(&self) -> usize {
        self.stale_temp_dirs.len() + self.partial_bundles.len()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} share-export debris under {}: {} stale temp dir(s), {} partial bundle(s)",
            self.total_entries(),
            self.archive_root.display(),
            self.stale_temp_dirs.len(),
            self.partial_bundles.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "stale_threshold_secs": self.stale_threshold_secs,
                "stale_temp_dirs": self.stale_temp_dirs,
                "partial_bundles": self.partial_bundles,
                "manual_remediation": {
                    "steps": [
                        "Confirm no live `am share` / `am doctor pack-archive` is in progress (their temp dirs are short-lived and should not appear in this finding).",
                        "For each stale temp dir: `mkdir -p .doctor/quarantine/share-debris && mv <stale_path> .doctor/quarantine/share-debris/` (preserves the in-progress bytes for forensics).",
                        "For each partial bundle: inspect the parent bundle dir (`ls -la <bundle_dir>`); a missing `mailbox.sqlite3` payload OR a `.partial` zip without a final `.zip` means the export was aborted. Move the entire bundle dir into the quarantine subtree — there's no safe way to finish it from outside the export pipeline.",
                        "If many debris entries accumulated, the share-export pipeline may be crashing on a specific input. Check `am robot status` and recent `tracing` logs for the underlying error before re-running `am share`.",
                        "Re-run `am doctor fix --only fm-share_export_state-half-finished-bundle-after-crash --list` to confirm zero residual debris.",
                    ],
                    "warning": "Partial bundles can be exposed through the share-export API as if they were valid (the API does not currently re-verify completeness on read). Move them aside as a P1 before the next consumer reads them.",
                    "safe_fix_deferred": "Auto-fix via Op::Rename to `<run-dir>/quarantine/share-debris/` is intentionally deferred in this first cut. The chokepoint already implements Op::Rename (see `stale_archive_lock` and `stale_head_or_ref_lock`); a follow-up pass wires per-entry quarantine plus a round-trip test (corrupt → fix → undo → debris reappears at the original path).",
                    "common_causes": [
                        "Kernel reboot or OOM kill during a long `am share` export.",
                        "Signal-terminated `mcp-agent-mail` server with an export-in-progress.",
                        "Filesystem ENOSPC mid-zip-flush leaves `bundle.zip.partial` behind.",
                        "Process killed AFTER manifest.json was written but BEFORE the mailbox.sqlite3 payload landed.",
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
    /// Override the stale-temp-dir age threshold (seconds).
    /// `None` uses `DEFAULT_STALE_THRESHOLD_SECS`.
    pub stale_threshold_secs_override: Option<u64>,
}

/// Detector. PURE w.r.t. the supplied project root.
///
/// Returns at most one aggregated finding per call. Returns
/// empty when `project_root` is `None`, when
/// `<project_root>/archived_mailbox_states/` doesn't exist, or
/// when no debris is found.
pub fn detect(
    project_root: Option<&Path>,
    inputs: &DetectInputs,
) -> Vec<ShareHalfFinishedBundleFinding> {
    let Some(root) = project_root else {
        return Vec::new();
    };
    let archive_root = root.join(ARCHIVE_STATES_DIR);
    if !archive_root.is_dir() {
        return Vec::new();
    }
    let stale_threshold_secs = inputs
        .stale_threshold_secs_override
        .unwrap_or(DEFAULT_STALE_THRESHOLD_SECS);
    let now = SystemTime::now();

    let stale_temp_dirs = collect_stale_temp_dirs(&archive_root, now, stale_threshold_secs);
    let partial_bundles = collect_partial_bundles(&archive_root);

    if stale_temp_dirs.is_empty() && partial_bundles.is_empty() {
        return Vec::new();
    }
    vec![ShareHalfFinishedBundleFinding {
        archive_root,
        stale_threshold_secs,
        stale_temp_dirs,
        partial_bundles,
    }]
}

fn collect_stale_temp_dirs(
    archive_root: &Path,
    now: SystemTime,
    stale_threshold_secs: u64,
) -> Vec<StaleTempDirEntry> {
    let mut out: Vec<StaleTempDirEntry> = Vec::new();
    walk_temp_dirs(
        archive_root,
        0,
        MAX_WALK_DEPTH,
        now,
        stale_threshold_secs,
        &mut out,
    );
    out
}

fn walk_temp_dirs(
    dir: &Path,
    depth: u32,
    max_depth: u32,
    now: SystemTime,
    stale_threshold_secs: u64,
    out: &mut Vec<StaleTempDirEntry>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if let Some(prefix) = matched_temp_pattern(name_str) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let age_secs = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
            if age_secs >= stale_threshold_secs {
                out.push(StaleTempDirEntry {
                    path: path.clone(),
                    age_secs,
                    matched_prefix: prefix.to_string(),
                });
            }
            // Don't recurse INTO a matched temp dir — its
            // contents are part of the same crash debris.
            continue;
        }
        // Recurse into a non-temp subdir (typical bundle dir)
        // — debris may be nested.
        walk_temp_dirs(&path, depth + 1, max_depth, now, stale_threshold_secs, out);
    }
}

fn matched_temp_pattern(name: &str) -> Option<&'static str> {
    if let Some(prefix) = LEGACY_TEMP_PREFIXES.iter().find(|p| name.starts_with(*p)) {
        return Some(*prefix);
    }
    if name.starts_with(".bundle-zip.") {
        return Some(".bundle-zip.");
    }
    if name.starts_with("mailbox-archive-zip-") {
        return Some("mailbox-archive-zip-");
    }
    if name.contains(".bundle-stage.") {
        return Some(".<bundle>.bundle-stage.");
    }
    if name.contains(".bundle-backup.") {
        return Some(".<bundle>.bundle-backup.");
    }
    None
}

fn collect_partial_bundles(archive_root: &Path) -> Vec<PartialBundleEntry> {
    let mut out: Vec<PartialBundleEntry> = Vec::new();
    let Ok(rd) = std::fs::read_dir(archive_root) else {
        return out;
    };
    for entry in rd.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let bundle_dir = entry.path();
        // Skip temp dirs — those are owned by the stale-temp-dir
        // walk above (avoid double-emit). let-chain (Rust 2024) keeps
        // clippy::collapsible_if happy.
        if let Some(name) = bundle_dir.file_name().and_then(|n| n.to_str())
            && matched_temp_pattern(name).is_some()
        {
            continue;
        }
        let manifest = bundle_dir.join("manifest.json");
        let database = bundle_dir.join("mailbox.sqlite3");
        if manifest.is_file() && !database.is_file() {
            out.push(PartialBundleEntry {
                bundle_dir: bundle_dir.clone(),
                reason: PartialBundleReason::MissingDatabase,
            });
            // Don't double-emit if the same bundle ALSO has a
            // .partial zip — one signal is sufficient.
            continue;
        }
        let zip_partial = bundle_dir.join("bundle.zip.partial");
        let zip_final = bundle_dir.join("bundle.zip");
        if zip_partial.exists() && !zip_final.exists() {
            out.push(PartialBundleEntry {
                bundle_dir,
                reason: PartialBundleReason::PartialZip,
            });
        }
    }
    out
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &ShareHalfFinishedBundleFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// In tests, drive the stale check via the threshold override
    /// (set to 0) rather than backdating mtimes — backdating
    /// directories needs platform-specific syscalls or extra deps
    /// (`filetime`) that aren't in the workspace. With
    /// `stale_threshold_secs_override: Some(0)`, the live mtime
    /// counts as "stale" instantly and the assertions still pin
    /// the right shape.
    fn stale_now_inputs() -> DetectInputs {
        DetectInputs {
            stale_threshold_secs_override: Some(0),
        }
    }

    /// **NEGATIVE TEST FIRST**: project_root=None → no finding.
    #[test]
    fn detector_returns_empty_for_no_project_root() {
        assert!(detect(None, &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: project root exists but no `archived_mailbox_states/`
    /// subdir → no finding.
    #[test]
    fn detector_returns_empty_when_archive_dir_absent() {
        let td = TempDir::new().unwrap();
        assert!(detect(Some(td.path()), &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: archive dir exists but is empty → no finding.
    #[test]
    fn detector_returns_empty_for_empty_archive_dir() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(ARCHIVE_STATES_DIR)).unwrap();
        assert!(detect(Some(td.path()), &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: a fresh temp dir (mtime now) must NOT flag —
    /// it could be a live export in progress.
    #[test]
    fn detector_skips_fresh_temp_dirs() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(arch.join("am-share-stage-12345")).unwrap();
        // mtime is "now" by default — should be well within
        // the stale threshold.
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert!(
            findings.is_empty(),
            "fresh temp dirs must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_stale_stage_temp_dir_with_threshold_zero() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(arch.join("am-share-stage-old")).unwrap();
        let findings = detect(Some(td.path()), &stale_now_inputs());
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.stale_temp_dirs.len(), 1);
        assert_eq!(f.stale_temp_dirs[0].matched_prefix, "am-share-stage-");
    }

    #[test]
    fn detector_flags_all_four_temp_prefixes() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        for prefix in LEGACY_TEMP_PREFIXES {
            fs::create_dir_all(arch.join(format!("{prefix}xyz"))).unwrap();
        }
        let findings = detect(Some(td.path()), &stale_now_inputs());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].stale_temp_dirs.len(), 4);
        let prefixes: std::collections::HashSet<String> = findings[0]
            .stale_temp_dirs
            .iter()
            .map(|e| e.matched_prefix.clone())
            .collect();
        for prefix in LEGACY_TEMP_PREFIXES {
            assert!(prefixes.contains(*prefix));
        }
    }

    #[test]
    fn detector_flags_current_stage_temp_dir_with_threshold_zero() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(arch.join(".bundle.bundle-stage.123.0")).unwrap();
        let findings = detect(Some(td.path()), &stale_now_inputs());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].stale_temp_dirs.len(), 1);
        assert_eq!(
            findings[0].stale_temp_dirs[0].matched_prefix,
            ".<bundle>.bundle-stage."
        );
    }

    #[test]
    fn detector_flags_partial_bundle_missing_database() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        let bundle = arch.join("bundle-2026-05-15");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"{}\n").unwrap();
        // No mailbox.sqlite3 — publish crashed after manifest but before DB payload.
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].partial_bundles.len(), 1);
        assert_eq!(
            findings[0].partial_bundles[0].reason,
            PartialBundleReason::MissingDatabase
        );
    }

    #[test]
    fn detector_skips_unsigned_but_complete_bundle() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        let bundle = arch.join("bundle-2026-05-15");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"{}\n").unwrap();
        fs::write(bundle.join("mailbox.sqlite3"), b"db\n").unwrap();
        // No manifest.sig.json — valid when --signing-key was not used.
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert!(
            findings.is_empty(),
            "unsigned complete bundle must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_partial_zip_without_final() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        let bundle = arch.join("bundle-2026-05-15");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"{}\n").unwrap();
        fs::write(bundle.join("mailbox.sqlite3"), b"db\n").unwrap();
        fs::write(bundle.join("bundle.zip.partial"), b"partial\n").unwrap();
        // No bundle.zip — zip writer crashed.
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].partial_bundles.len(), 1);
        assert_eq!(
            findings[0].partial_bundles[0].reason,
            PartialBundleReason::PartialZip
        );
    }

    /// **NEGATIVE**: a healthy completed bundle (manifest + DB payload +
    /// final zip) must NOT flag.
    #[test]
    fn detector_skips_healthy_completed_bundle() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        let bundle = arch.join("bundle-2026-05-15");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"{}\n").unwrap();
        fs::write(bundle.join("mailbox.sqlite3"), b"db\n").unwrap();
        fs::write(bundle.join("bundle.zip"), b"final\n").unwrap();
        let findings = detect(Some(td.path()), &DetectInputs::default());
        assert!(
            findings.is_empty(),
            "healthy bundle must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_aggregates_temp_dirs_and_partial_bundles_in_one_finding() {
        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(arch.join("am-share-finalize-abc")).unwrap();
        let bundle = arch.join("bundle-2026-05-15");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"{}\n").unwrap();
        let findings = detect(Some(td.path()), &stale_now_inputs());
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.stale_temp_dirs.len(), 1);
        assert_eq!(f.partial_bundles.len(), 1);
        assert_eq!(f.total_entries(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn detector_does_not_follow_symlinked_archive_subdir() {
        use std::os::unix::fs::symlink;

        let td = TempDir::new().unwrap();
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(&arch).unwrap();
        let outside = td.path().join("outside");
        fs::create_dir_all(outside.join("am-share-stage-old")).unwrap();
        symlink(&outside, arch.join("linked-outside")).unwrap();

        assert!(
            detect(Some(td.path()), &stale_now_inputs()).is_empty(),
            "doctor must not follow symlinked bundle dirs while scanning share debris"
        );
    }

    #[test]
    fn finding_serializes_with_reason_strings_and_remediation() {
        let f = ShareHalfFinishedBundleFinding {
            archive_root: "/tmp/x".into(),
            stale_threshold_secs: 3600,
            stale_temp_dirs: vec![StaleTempDirEntry {
                path: "/tmp/x/am-share-stage-1".into(),
                age_secs: 7200,
                matched_prefix: "am-share-stage-".to_string(),
            }],
            partial_bundles: vec![PartialBundleEntry {
                bundle_dir: "/tmp/x/bundle-1".into(),
                reason: PartialBundleReason::MissingDatabase,
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"missing_database\""));
        assert!(s.contains("\"stale_threshold_secs\":3600"));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am doctor explain"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
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
        let finding = ShareHalfFinishedBundleFinding {
            archive_root: td.path().to_path_buf(),
            stale_threshold_secs: 3600,
            stale_temp_dirs: Vec::new(),
            partial_bundles: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
