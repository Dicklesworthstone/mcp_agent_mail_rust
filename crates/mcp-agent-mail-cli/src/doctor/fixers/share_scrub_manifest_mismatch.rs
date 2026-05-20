//! `fm-share_export_state-scrub-marker-manifest-mismatch` — P1
//! detect-only.
//!
//! **Subsystem**: share_export_state.
//!
//! ## What's broken
//!
//! Each published share bundle under
//! `<project_root>/archived_mailbox_states/<bundle>/` records the
//! scrubber's output in two places that MUST agree with the
//! actual on-disk artifact set:
//!
//! - `manifest.json` — under unique
//!   `attachments.items[].bundle_path` values for the current
//!   bundle schema, with fallbacks to `attachments.stats.copied`
//!   and legacy `scrub_summary.counts.attachments_kept`.
//! - `viewer_data.json` — under `attachments_total` for older
//!   bundle shapes that wrote a separate viewer-data file.
//!
//! When the actual `<bundle_dir>/attachments/` tree disagrees
//! with either count, the bundle is "scrubber-drift" broken:
//!
//! - **AttachmentCountMismatch**: the manifest's copied/kept
//!   attachment count differs from
//!   `count_files_recursive(<bundle>/attachments/)`.
//!   Operator consumers reading the manifest get a count that
//!   doesn't match what they'd see on disk.
//! - **ViewerCountMismatch**: `viewer_data.attachments_total`
//!   differs from the actual count. The viewer panel shows the
//!   wrong number; users may think attachments were dropped /
//!   added that weren't.
//!
//! Causes: a crash between manifest write + attachment finalize,
//! a bundle that was partially edited after publish, or a
//! migration that rewrote the manifest without re-scrubbing.
//!
//! Distinct from `share_half_finished_bundle` (FM23) which
//! catches debris from in-progress crashes. THIS FM applies to
//! bundles that LOOK fully published (have manifest + sig +
//! zip / attachments) but whose internal counts have drifted.
//!
//! ## Detection (pure)
//!
//! For each immediate subdir under
//! `<project_root>/archived_mailbox_states/` (skip `am-share-*`
//! temp dirs — those are FM23's territory):
//!
//! 1. If no `manifest.json`, skip — this FM applies to bundles
//!    with a manifest in place.
//! 2. Parse `manifest.json`. If parse fails, skip — a malformed
//!    manifest is a different FM concern.
//! 3. Count files recursively under `<bundle>/attachments/`
//!    (0 if the dir is absent).
//! 4. Read the manifest's claimed on-disk attachment file count.
//!    Current bundles use unique `attachments.items[].bundle_path`
//!    values because duplicate attachments are content-deduped on
//!    disk. Fallbacks: `attachments.stats.copied`, then legacy
//!    `scrub_summary.counts.attachments_kept`. If present and it
//!    doesn't equal the recursive count, record an
//!    `AttachmentCountMismatch`.
//! 5. If `viewer_data.json` exists, parse it and read
//!    `attachments_total`. If present and doesn't equal the
//!    recursive count, record a `ViewerCountMismatch`.
//!
//! ## Fix
//!
//! **Detect-only.** Auto-fix would have to either:
//! (a) rewrite the manifest counts to match the disk state
//!     (dangerous — silently masks a real scrub bug), or
//! (b) re-run the scrubber against the bundle's source data
//!     (out of scope for the doctor; needs the export pipeline).
//!
//! Manual remediation: re-export the bundle from source via
//! `am share` and move the drifted bundle aside; the drifted
//! counts cannot be "fixed" in place without re-scrubbing,
//! which the doctor doesn't ship.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-share_export_state-scrub-marker-manifest-mismatch";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "share_export_state";

const ARCHIVE_STATES_DIR: &str = "archived_mailbox_states";

/// Legacy temp-dir prefixes that FM23 owns; we deliberately skip them
/// here to avoid double-emission on debris.
const LEGACY_SKIP_PREFIXES: &[&str] = &[
    "am-share-finalize-rollback-",
    "am-share-finalize-",
    "am-share-stage-",
    "am-share-backup-",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Unique `manifest.attachments.items[].bundle_path` values (or
    /// current/legacy count fallbacks) != disk count.
    AttachmentCountMismatch,
    /// `viewer_data.attachments_total` ≠ disk count.
    ViewerCountMismatch,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftEntry {
    pub bundle_dir: PathBuf,
    pub kind: DriftKind,
    pub claimed: u64,
    pub actual: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareScrubManifestMismatchFinding {
    pub archive_root: PathBuf,
    pub entries: Vec<DriftEntry>,
}

impl ShareScrubManifestMismatchFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} share bundle(s) under {} have scrubber-drift between manifest counts and on-disk attachments",
            self.entries.len(),
            self.archive_root.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "entries": self.entries,
                "manual_remediation": {
                    "steps": [
                        "For each entry: inspect both sides. `jq '.attachments.items[].bundle_path, .attachments.stats, .scrub_summary.counts' <bundle>/manifest.json` shows the manifest's claim; `find <bundle>/attachments -type f | wc -l` shows the disk truth.",
                        "Decide which side is authoritative. If the disk is correct, the manifest is stale — re-export via `am share` to regenerate a fresh manifest + viewer_data + signature.",
                        "If the manifest is correct, the disk has drifted — preserve forensics (`mv <bundle> .doctor/quarantine/scrub-drift/`) and re-export rather than patching files in place.",
                        "Re-run `am doctor fix --only fm-share_export_state-scrub-marker-manifest-mismatch --list` to confirm zero residual drift.",
                    ],
                    "warning": "The drift means the manifest's scrub summary cannot be trusted as a description of the bundle. Downstream consumers reading `attachments_kept` / `attachments_total` get a wrong number — silently.",
                    "safe_fix_deferred": "Auto-fix is intentionally NOT implemented. Rewriting the manifest counts in place would silently mask the underlying scrub-pipeline bug; re-running the scrubber requires the export pipeline (out of doctor scope). Operator-driven re-export is the canonical recovery.",
                    "common_causes": [
                        "Crash between manifest write and attachment-finalize step (manifest landed with the planned count, finalize aborted before the actual files matched).",
                        "Manual edit of a published bundle (operator added/removed attachments without re-scrubbing).",
                        "Migration script that rewrote manifests without re-running the scrubber.",
                        "Bundle restored from a backup where manifest and attachments were captured at different times.",
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

/// Detector. PURE w.r.t. the supplied project root.
pub fn detect(project_root: Option<&Path>) -> Vec<ShareScrubManifestMismatchFinding> {
    let Some(root) = project_root else {
        return Vec::new();
    };
    let archive_root = root.join(ARCHIVE_STATES_DIR);
    if !archive_root.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<DriftEntry> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&archive_root) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let bundle_dir = entry.path();
        if let Some(name) = bundle_dir.file_name().and_then(|n| n.to_str())
            && is_share_temp_dir_name(name)
        {
            continue;
        }
        check_bundle(&bundle_dir, &mut entries);
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![ShareScrubManifestMismatchFinding {
        archive_root,
        entries,
    }]
}

fn is_share_temp_dir_name(name: &str) -> bool {
    LEGACY_SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
        || name.starts_with(".bundle-zip.")
        || name.starts_with("mailbox-archive-zip-")
        || name.contains(".bundle-stage.")
        || name.contains(".bundle-backup.")
}

fn check_bundle(bundle_dir: &Path, out: &mut Vec<DriftEntry>) {
    let manifest_path = bundle_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return;
    }
    let Ok(manifest_body) = std::fs::read_to_string(&manifest_path) else {
        return;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_body) else {
        return;
    };

    let attachments_dir = bundle_dir.join("attachments");
    let actual_attachments = count_files_recursive(&attachments_dir);

    if let Some(claimed) = manifest_claimed_attachment_files(&manifest)
        && claimed != actual_attachments
    {
        out.push(DriftEntry {
            bundle_dir: bundle_dir.to_path_buf(),
            kind: DriftKind::AttachmentCountMismatch,
            claimed,
            actual: actual_attachments,
        });
    }

    let viewer_path = bundle_dir.join("viewer_data.json");
    if viewer_path.is_file()
        && let Ok(viewer_body) = std::fs::read_to_string(&viewer_path)
        && let Ok(viewer) = serde_json::from_str::<serde_json::Value>(&viewer_body)
        && let Some(claimed) = viewer.get("attachments_total").and_then(|v| v.as_u64())
        && claimed != actual_attachments
    {
        out.push(DriftEntry {
            bundle_dir: bundle_dir.to_path_buf(),
            kind: DriftKind::ViewerCountMismatch,
            claimed,
            actual: actual_attachments,
        });
    }
}

fn manifest_claimed_attachment_files(manifest: &serde_json::Value) -> Option<u64> {
    if let Some(items) = manifest
        .get("attachments")
        .and_then(|a| a.get("items"))
        .and_then(|v| v.as_array())
    {
        let unique_bundle_paths: std::collections::BTreeSet<&str> = items
            .iter()
            .filter_map(|item| {
                let mode = item.get("mode").and_then(|v| v.as_str());
                let bundle_path = item.get("bundle_path").and_then(|v| v.as_str());
                match (mode, bundle_path) {
                    (Some("file"), Some(path)) | (None, Some(path)) => Some(path),
                    _ => None,
                }
            })
            .collect();
        if !unique_bundle_paths.is_empty() {
            return Some(unique_bundle_paths.len() as u64);
        }
        if let Some(copied) = manifest_current_stats_copied(manifest)
            && copied > 0
        {
            return Some(copied);
        }
        return Some(0);
    }

    manifest_current_stats_copied(manifest).or_else(|| {
        manifest
            .get("scrub_summary")
            .and_then(|s| s.get("counts"))
            .and_then(|c| c.get("attachments_kept"))
            .and_then(|v| v.as_u64())
    })
}

fn manifest_current_stats_copied(manifest: &serde_json::Value) -> Option<u64> {
    manifest
        .get("attachments")
        .and_then(|a| a.get("stats"))
        .and_then(|s| s.get("copied"))
        .and_then(|v| v.as_u64())
}

fn count_files_recursive(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let mut count: u64 = 0;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            count = count.saturating_add(count_files_recursive(&entry.path()));
        } else if meta.is_file() {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &ShareScrubManifestMismatchFinding,
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

    fn write_manifest(bundle: &Path, attachments_kept: Option<u64>) {
        let body = match attachments_kept {
            Some(n) => format!(r#"{{"scrub_summary":{{"counts":{{"attachments_kept":{n}}}}}}}"#),
            None => "{}".to_string(),
        };
        fs::write(bundle.join("manifest.json"), body).unwrap();
    }

    fn write_current_manifest(bundle: &Path, copied: u64) {
        let body = format!(r#"{{"attachments":{{"stats":{{"copied":{copied}}}}}}}"#);
        fs::write(bundle.join("manifest.json"), body).unwrap();
    }

    fn write_current_manifest_with_items(bundle: &Path, copied: u64, bundle_paths: &[&str]) {
        let items = bundle_paths
            .iter()
            .map(|path| serde_json::json!({"mode": "file", "bundle_path": path}))
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "attachments": {
                "stats": {"copied": copied},
                "items": items,
            },
        });
        fs::write(
            bundle.join("manifest.json"),
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
    }

    fn write_viewer(bundle: &Path, attachments_total: u64) {
        let body = format!(r#"{{"attachments_total":{attachments_total}}}"#);
        fs::write(bundle.join("viewer_data.json"), body).unwrap();
    }

    fn plant_attachments(bundle: &Path, count: u64) {
        let dir = bundle.join("attachments");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..count {
            fs::write(dir.join(format!("att-{i}")), b"x").unwrap();
        }
    }

    fn make_archive(td: &TempDir) -> PathBuf {
        let arch = td.path().join(ARCHIVE_STATES_DIR);
        fs::create_dir_all(&arch).unwrap();
        arch
    }

    /// **NEGATIVE TEST FIRST**: no project root → no finding.
    #[test]
    fn detector_returns_empty_for_no_project_root() {
        assert!(detect(None).is_empty());
    }

    /// **NEGATIVE**: archive dir doesn't exist → no finding.
    #[test]
    fn detector_returns_empty_when_archive_dir_absent() {
        let td = TempDir::new().unwrap();
        assert!(detect(Some(td.path())).is_empty());
    }

    /// **NEGATIVE**: bundle has no manifest → no finding (this FM
    /// applies only to bundles with a manifest in place).
    #[test]
    fn detector_returns_empty_for_bundle_without_manifest() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 5);
        assert!(detect(Some(td.path())).is_empty());
    }

    /// **NEGATIVE**: manifest is malformed → skip silently (a
    /// different FM owns malformed-manifest detection).
    #[test]
    fn detector_skips_malformed_manifest() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"not json at all").unwrap();
        plant_attachments(&bundle, 0);
        assert!(detect(Some(td.path())).is_empty());
    }

    /// **NEGATIVE**: a healthy bundle (counts match) → no finding.
    #[test]
    fn detector_returns_empty_for_healthy_bundle() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 3);
        write_manifest(&bundle, Some(3));
        write_viewer(&bundle, 3);
        let findings = detect(Some(td.path()));
        assert!(
            findings.is_empty(),
            "healthy bundle must not flag: {findings:?}"
        );
    }

    /// **NEGATIVE**: manifest lacks `scrub_summary.counts.attachments_kept`
    /// AND no `viewer_data.json` → no finding (nothing to compare).
    #[test]
    fn detector_returns_empty_when_no_count_fields_present() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 3);
        write_manifest(&bundle, None);
        // No viewer_data.json — nothing to compare.
        assert!(detect(Some(td.path())).is_empty());
    }

    /// **NEGATIVE**: skip share-export temp dirs (FM23's territory).
    #[test]
    fn detector_skips_share_export_temp_dirs() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        // Plant a temp dir with a manifest that WOULD mismatch
        // (claimed=99, actual=0). FM23 owns this; this FM must
        // not flag it.
        let bundle = arch.join(".bundle.bundle-stage.123.0");
        fs::create_dir_all(&bundle).unwrap();
        write_manifest(&bundle, Some(99));
        assert!(
            detect(Some(td.path())).is_empty(),
            "am-share-* temp dirs are owned by FM23, not this FM"
        );
    }

    #[test]
    fn detector_flags_attachment_count_mismatch() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 3);
        write_manifest(&bundle, Some(99)); // claimed=99, actual=3
        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].kind, DriftKind::AttachmentCountMismatch);
        assert_eq!(f.entries[0].claimed, 99);
        assert_eq!(f.entries[0].actual, 3);
    }

    #[test]
    fn detector_flags_current_manifest_attachment_stats_mismatch() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 2);
        write_current_manifest(&bundle, 4);
        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entries[0].kind,
            DriftKind::AttachmentCountMismatch
        );
        assert_eq!(findings[0].entries[0].claimed, 4);
        assert_eq!(findings[0].entries[0].actual, 2);
    }

    #[test]
    fn detector_counts_current_manifest_unique_bundle_paths_to_allow_dedup() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(bundle.join("attachments/ab")).unwrap();
        fs::write(bundle.join("attachments/ab/digest.txt"), b"x").unwrap();
        write_current_manifest_with_items(
            &bundle,
            2,
            &["attachments/ab/digest.txt", "attachments/ab/digest.txt"],
        );

        let findings = detect(Some(td.path()));
        assert!(
            findings.is_empty(),
            "deduped attachment references should not look like on-disk drift: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_current_manifest_unique_bundle_path_missing_on_disk() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        write_current_manifest_with_items(&bundle, 1, &["attachments/ab/missing.txt"]);

        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entries[0].kind,
            DriftKind::AttachmentCountMismatch
        );
        assert_eq!(findings[0].entries[0].claimed, 1);
        assert_eq!(findings[0].entries[0].actual, 0);
    }

    #[test]
    fn detector_falls_back_to_stats_when_items_have_no_bundle_paths() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        let manifest = serde_json::json!({
            "attachments": {
                "stats": {"copied": 2},
                "items": [
                    {"mode": "file"},
                    {"mode": "external", "original_path": "/tmp/large.bin"}
                ],
            },
        });
        fs::write(
            bundle.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].claimed, 2);
        assert_eq!(findings[0].entries[0].actual, 0);
    }

    #[test]
    fn detector_flags_viewer_count_mismatch() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 5);
        write_manifest(&bundle, Some(5)); // manifest agrees
        write_viewer(&bundle, 17); // viewer disagrees
        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 1);
        assert_eq!(findings[0].entries[0].kind, DriftKind::ViewerCountMismatch);
        assert_eq!(findings[0].entries[0].claimed, 17);
        assert_eq!(findings[0].entries[0].actual, 5);
    }

    #[test]
    fn detector_emits_both_kinds_when_both_disagree() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        let bundle = arch.join("bundle-1");
        fs::create_dir_all(&bundle).unwrap();
        plant_attachments(&bundle, 2);
        write_manifest(&bundle, Some(10));
        write_viewer(&bundle, 7);
        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 2);
        let kinds: std::collections::HashSet<DriftKind> =
            findings[0].entries.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&DriftKind::AttachmentCountMismatch));
        assert!(kinds.contains(&DriftKind::ViewerCountMismatch));
    }

    #[test]
    fn count_files_recursive_handles_nested_dirs() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("top1"), b"x").unwrap();
        fs::write(root.join("a/mid1"), b"x").unwrap();
        fs::write(root.join("a/b/deep1"), b"x").unwrap();
        fs::write(root.join("a/b/c/deepest1"), b"x").unwrap();
        assert_eq!(count_files_recursive(root), 4);
    }

    #[cfg(unix)]
    #[test]
    fn count_files_recursive_does_not_follow_symlinked_dirs() {
        use std::os::unix::fs::symlink;

        let td = TempDir::new().unwrap();
        let root = td.path().join("attachments");
        let outside = td.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("real"), b"x").unwrap();
        fs::write(outside.join("outside-file"), b"x").unwrap();
        symlink(&outside, root.join("linked-outside")).unwrap();

        assert_eq!(
            count_files_recursive(&root),
            1,
            "attachment counting must not escape through symlinked directories"
        );
    }

    #[test]
    fn count_files_recursive_returns_zero_for_missing_dir() {
        assert_eq!(count_files_recursive(Path::new("/no/such/path")), 0);
    }

    #[test]
    fn detector_aggregates_drifts_across_multiple_bundles() {
        let td = TempDir::new().unwrap();
        let arch = make_archive(&td);
        for i in 0..3 {
            let bundle = arch.join(format!("bundle-{i}"));
            fs::create_dir_all(&bundle).unwrap();
            plant_attachments(&bundle, i as u64);
            write_manifest(&bundle, Some(100 + i as u64));
        }
        let findings = detect(Some(td.path()));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[test]
    fn finding_serializes_with_kind_strings_and_remediation() {
        let f = ShareScrubManifestMismatchFinding {
            archive_root: "/tmp/x".into(),
            entries: vec![
                DriftEntry {
                    bundle_dir: "/tmp/x/bundle-1".into(),
                    kind: DriftKind::AttachmentCountMismatch,
                    claimed: 10,
                    actual: 3,
                },
                DriftEntry {
                    bundle_dir: "/tmp/x/bundle-1".into(),
                    kind: DriftKind::ViewerCountMismatch,
                    claimed: 7,
                    actual: 3,
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"attachment_count_mismatch\""));
        assert!(s.contains("\"viewer_count_mismatch\""));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am share"));
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
        let finding = ShareScrubManifestMismatchFinding {
            archive_root: td.path().to_path_buf(),
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
