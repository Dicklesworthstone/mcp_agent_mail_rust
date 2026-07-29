//! `fm-archive-state-files-stale-tmp-pack-from-interrupted-gc` — P2.
//!
//! **Subsystem**: archive_state_files (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! When `git gc` / `git repack` / `am doctor pack-archive` builds a new
//! pack, `pack-objects` first streams the candidate pack to a temporary
//! file named `<repo>/.git/objects/pack/tmp_pack_XXXXXX`, then atomically
//! renames it into place once the pack + index are complete. If the host
//! is hard-rebooted (or the packer is SIGKILLed / OOM-killed) mid-repack,
//! the `tmp_pack_*` file is orphaned: git never resumes that specific temp
//! file, and nothing cleans it up automatically (`git gc` only prunes
//! `tmp_pack_*` older than `gc.pruneExpire` *while it actually runs*, and
//! on a wedged archive gc may never run again).
//!
//! Observed in the ts2 incident: a hard reboot during gc left **~1,327**
//! `tmp_pack_*` files in a single project archive. They are pure dead
//! weight — wasted disk + inode pressure that slows every `readdir` on the
//! pack directory and inflates backups. No existing doctor FM scanned for
//! them (grep across the fixer tree returned zero matches before this FM).
//!
//! P2 (not P1) because, unlike a stale `index.lock`, an orphaned
//! `tmp_pack_*` does **not** block git operations — it only wastes space
//! and degrades pack-dir traversal. It is more than P3 perf, though,
//! because a reboot-during-gc can leave thousands of them and they never
//! self-heal.
//!
//! ## Detection (pure)
//!
//! For every project archive root:
//! 1. Locate `<archive>/.git/objects/pack/`.
//! 2. Enumerate regular files whose name starts with `tmp_pack_`
//!    (symlinks are never followed — a symlink there is a tampering
//!    signal handled by the dedicated symlink FM, not us).
//! 3. Keep only files whose mtime is older than `stale_seconds` (default
//!    [`DEFAULT_STALE_SECONDS`] = 1 hour). A live `git repack` streams to
//!    its `tmp_pack_*` continuously, so its mtime stays fresh; a file that
//!    has not been touched for an hour is definitively abandoned. This is
//!    the conservative direction — we never quarantine a temp file that an
//!    in-flight packer might still be writing.
//! 4. Emit ONE aggregated finding per affected repo listing the stale
//!    files (capped sample in evidence + full count/bytes).
//!
//! ## Fix
//!
//! Auto-fixable. For each stale file: `mutate(ctx, path, Op::Rename { to:
//! quarantine })` where the quarantine path is
//! `<run-dir>/quarantine/<archive-slug>/<tmp_pack_name>.<ns>`. Per
//! AGENTS.md RULE 1 we never delete — the orphaned temp packs are
//! preserved under the run dir so an operator can inspect them or reclaim
//! space deliberately. `am doctor undo <run-id>` reverses every rename.
//!
//! ## Reversibility
//!
//! Each quarantine is a hash-witnessed `Op::Rename` recorded in
//! `actions.jsonl`; `am doctor undo` restores byte-identical originals and
//! refuses to clobber a recreated file (a fresh repack putting a new
//! `tmp_pack_*` in place is left untouched).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-archive-state-files-stale-tmp-pack-from-interrupted-gc";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Default mtime-based staleness threshold (1 hour). An in-flight
/// `git repack` advances its `tmp_pack_*` mtime as it streams the pack, so
/// only files untouched for at least this long are treated as orphaned. No
/// legitimate mailbox-archive repack runs for an hour, so this is safely
/// conservative against killing an in-progress packer.
pub const DEFAULT_STALE_SECONDS: u64 = 3600;

/// How many sample paths to embed in the finding evidence. A
/// reboot-during-gc can leave thousands of files; the report records the
/// full count + total bytes but only a bounded sample of paths so
/// `report.json` (and any operator who cats it) is not flooded.
const EVIDENCE_SAMPLE_CAP: usize = 20;

/// One aggregated finding per affected project archive.
#[derive(Debug, Clone, Serialize)]
pub struct StaleTmpPackFinding {
    pub archive_root: PathBuf,
    /// Every stale `tmp_pack_*` file in this repo (the fixer quarantines
    /// all of them; evidence serialization caps the sample).
    pub tmp_pack_paths: Vec<PathBuf>,
    pub total_bytes: u64,
    pub oldest_age_seconds: u64,
}

impl StaleTmpPackFinding {
    pub fn count(&self) -> usize {
        self.tmp_pack_paths.len()
    }

    /// Project the typed payload back into the generic `Finding` envelope.
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} stale tmp_pack_* file(s) ({} bytes, oldest {}s) under {} — orphaned by an interrupted gc/repack",
            self.count(),
            self.total_bytes,
            self.oldest_age_seconds,
            self.archive_root.join(".git/objects/pack").display(),
        );
        let sample: Vec<String> = self
            .tmp_pack_paths
            .iter()
            .take(EVIDENCE_SAMPLE_CAP)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.99,
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "pack_dir": self.archive_root.join(".git/objects/pack").to_string_lossy(),
                "stale_count": self.count(),
                "total_bytes": self.total_bytes,
                "oldest_age_seconds": self.oldest_age_seconds,
                "sample_paths": sample,
                "sample_truncated": self.count() > EVIDENCE_SAMPLE_CAP,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: self.count(),
            },
        }
    }
}

/// Detector. PURE — no `mutate()` calls, no writes.
///
/// `archive_roots` is the list of per-project archive dirs
/// (`<storage_root>/projects/<slug>/`). `stale_seconds` is the mtime
/// threshold; production callers pass [`DEFAULT_STALE_SECONDS`].
pub fn detect(archive_roots: &[PathBuf], stale_seconds: u64) -> Vec<StaleTmpPackFinding> {
    let now = std::time::SystemTime::now();
    let mut out = Vec::new();
    for archive in archive_roots {
        let pack_dir = archive.join(".git").join("objects").join("pack");
        let Ok(rd) = fs::read_dir(&pack_dir) else {
            continue; // no pack dir (or unreadable) — nothing to scan
        };

        let mut paths: Vec<PathBuf> = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut oldest_age_seconds: u64 = 0;

        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !name_str.starts_with("tmp_pack_") {
                continue;
            }
            // symlink_metadata: never follow a symlink (a symlink in the
            // pack dir is a tampering signal owned by the symlink FM).
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let age_seconds = meta
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if age_seconds < stale_seconds {
                // Possibly an in-flight repack still streaming — leave it.
                continue;
            }
            total_bytes = total_bytes.saturating_add(meta.len());
            oldest_age_seconds = oldest_age_seconds.max(age_seconds);
            paths.push(entry.path());
        }

        if paths.is_empty() {
            continue;
        }
        // Deterministic order for stable reports + undo replay.
        paths.sort();
        out.push(StaleTmpPackFinding {
            archive_root: archive.clone(),
            tmp_pack_paths: paths,
            total_bytes,
            oldest_age_seconds,
        });
    }
    out
}

/// Fixer. Quarantines every stale `tmp_pack_*` via the `mutate()`
/// chokepoint (`Op::Rename`, never delete — per RULE 1). Idempotent: a
/// file that vanished between detect and fix (a fresh gc cleaned it, or a
/// prior run already moved it) is skipped.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleTmpPackFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let archive_slug = finding
        .archive_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown-archive".to_string());

    let mut actions_taken = 0usize;
    let mut actions_skipped = 0usize;
    let mut quarantined_paths = Vec::new();

    for src in &finding.tmp_pack_paths {
        if !src.exists() {
            actions_skipped += 1;
            continue;
        }
        let file_name = src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tmp_pack_unknown".to_string());
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let quarantine = ctx
            .run_dir
            .join("quarantine")
            .join(&archive_slug)
            .join(format!("{file_name}.{now_ns}"));

        mutate(
            ctx,
            src,
            Op::Rename {
                to: quarantine.clone(),
            },
        )?;
        actions_taken += 1;
        quarantined_paths.push(quarantine);
    }

    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
        quarantined_paths,
    })
}

/// Helper used by tests + callers wanting a quick "is this a tmp_pack temp
/// file" predicate without re-deriving the prefix rule.
pub fn is_tmp_pack_name(name: &Path) -> bool {
    name.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("tmp_pack_"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use crate::doctor::runs::scaffold_run_dir;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir: run_dir.clone(),
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    fn make_pack_dir(td: &TempDir, slug: &str) -> PathBuf {
        let archive = td.path().join(slug);
        fs::create_dir_all(archive.join(".git").join("objects").join("pack")).unwrap();
        archive
    }

    fn plant_tmp_pack(archive: &Path, name: &str, contents: &[u8], age_seconds: u64) -> PathBuf {
        let p = archive.join(".git").join("objects").join("pack").join(name);
        fs::write(&p, contents).unwrap();
        // Backdate mtime so the mtime-staleness gate fires deterministically.
        // Uses only std (`fs::FileTimes` + `File::set_times`, stable) — the
        // same idiom as `dangling_doctor_latest`'s tests; no external crate.
        if age_seconds > 0 {
            let older = std::time::SystemTime::now() - Duration::from_secs(age_seconds);
            let times = fs::FileTimes::new().set_modified(older);
            let f = fs::File::options().write(true).open(&p).unwrap();
            f.set_times(times).unwrap();
        }
        p
    }

    #[test]
    fn detector_returns_empty_when_no_pack_dir() {
        let td = TempDir::new().unwrap();
        let archive = td.path().join("alpha");
        fs::create_dir_all(archive.join(".git")).unwrap();
        assert!(detect(&[archive], DEFAULT_STALE_SECONDS).is_empty());
    }

    #[test]
    fn detector_returns_empty_when_only_real_packs() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        let pack = archive.join(".git/objects/pack");
        fs::write(pack.join("pack-deadbeef.pack"), b"PACK").unwrap();
        fs::write(pack.join("pack-deadbeef.idx"), b"IDX").unwrap();
        assert!(detect(&[archive], DEFAULT_STALE_SECONDS).is_empty());
    }

    #[test]
    fn detector_skips_fresh_tmp_pack() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        // mtime is "now" → an in-flight repack might still own it.
        plant_tmp_pack(&archive, "tmp_pack_abc123", b"streaming", 0);
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty(), "fresh tmp_pack must be left alone");
    }

    #[test]
    fn detector_flags_stale_tmp_pack() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        plant_tmp_pack(&archive, "tmp_pack_abc123", b"orphaned-pack-data", 7200);
        // Also plant a real pack which must be ignored.
        fs::write(
            archive.join(".git/objects/pack/pack-cafe.pack"),
            b"REALPACK",
        )
        .unwrap();
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.count(), 1);
        assert_eq!(f.total_bytes, b"orphaned-pack-data".len() as u64);
        assert!(f.oldest_age_seconds >= 7200);
        assert_eq!(f.archive_root, archive);
    }

    #[test]
    fn detector_aggregates_multiple_stale_files_in_one_finding() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        for i in 0..5 {
            plant_tmp_pack(&archive, &format!("tmp_pack_{i:06x}"), b"xxxx", 7200);
        }
        // a fresh one mixed in must NOT be counted.
        plant_tmp_pack(&archive, "tmp_pack_fresh", b"yy", 0);
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].count(), 5);
    }

    #[test]
    fn detector_honors_threshold_override() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        plant_tmp_pack(&archive, "tmp_pack_recent", b"z", 30);
        // Default 3600s → skipped; override to 10s → flagged.
        assert!(detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS).is_empty());
        let findings = detect(std::slice::from_ref(&archive), 10);
        assert_eq!(findings.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn detector_does_not_follow_symlinked_tmp_pack() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        let outside = td.path().join("outside-pack");
        fs::write(&outside, b"x").unwrap();
        let link = archive.join(".git/objects/pack/tmp_pack_link");
        symlink(&outside, &link).unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert!(
            findings.is_empty(),
            "symlinked tmp_pack must not be flagged"
        );
    }

    #[test]
    fn finding_serializes_with_required_fields_and_caps_sample() {
        let mut paths = Vec::new();
        for i in 0..(EVIDENCE_SAMPLE_CAP + 5) {
            paths.push(PathBuf::from(format!("/x/.git/objects/pack/tmp_pack_{i}")));
        }
        let f = StaleTmpPackFinding {
            archive_root: PathBuf::from("/x"),
            tmp_pack_paths: paths,
            total_bytes: 4096,
            oldest_age_seconds: 9000,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "archive_state_files");
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, EVIDENCE_SAMPLE_CAP + 5);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"sample_truncated\":true"));
        // sample is capped
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let sample = v["evidence"]["sample_paths"].as_array().unwrap();
        assert_eq!(sample.len(), EVIDENCE_SAMPLE_CAP);
        assert_eq!(
            v["evidence"]["stale_count"],
            (EVIDENCE_SAMPLE_CAP + 5) as i64
        );
    }

    #[test]
    fn fixer_quarantines_all_stale_files_via_mutate() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        let p1 = plant_tmp_pack(&archive, "tmp_pack_aaa", b"one", 7200);
        let p2 = plant_tmp_pack(&archive, "tmp_pack_bbb", b"two", 7200);
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);

        let run_id = "2026-06-18T20-00-00Z__tmppack";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 2);
        assert_eq!(outcome.actions_skipped, 0);
        assert_eq!(outcome.quarantined_paths.len(), 2);

        assert!(!p1.exists(), "tmp_pack must be moved out of pack dir");
        assert!(!p2.exists(), "tmp_pack must be moved out of pack dir");
        for q in &outcome.quarantined_paths {
            assert!(q.exists(), "quarantined file must exist: {}", q.display());
        }
    }

    #[test]
    fn fixer_idempotent_when_files_already_gone() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        let finding = StaleTmpPackFinding {
            archive_root: archive.clone(),
            tmp_pack_paths: vec![
                archive.join(".git/objects/pack/tmp_pack_gone1"),
                archive.join(".git/objects/pack/tmp_pack_gone2"),
            ],
            total_bytes: 0,
            oldest_age_seconds: 7200,
        };
        let run_id = "2026-06-18T20-00-01Z__gone";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 2);
        assert!(outcome.quarantined_paths.is_empty());
    }

    #[test]
    fn fixer_then_undo_restores_tmp_packs() {
        let td = TempDir::new().unwrap();
        let archive = make_pack_dir(&td, "alpha");
        let p1 = plant_tmp_pack(&archive, "tmp_pack_aaa", b"one", 7200);
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        let run_id = "2026-06-18T20-00-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        drop(ctx);
        assert!(!p1.exists());

        let summary = crate::doctor::undo::run_undo_with_scopes(
            td.path(),
            run_id,
            false,
            true,
            &[td.path().to_path_buf()],
        )
        .expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        assert!(p1.exists(), "undo must restore the tmp_pack file");
        assert_eq!(fs::read_to_string(&p1).unwrap(), "one");
    }

    #[test]
    fn is_tmp_pack_name_matches_only_prefix() {
        assert!(is_tmp_pack_name(Path::new("/x/tmp_pack_abc")));
        assert!(!is_tmp_pack_name(Path::new("/x/pack-abc.pack")));
        assert!(!is_tmp_pack_name(Path::new("/x/tmp_idx_abc")));
    }
}
