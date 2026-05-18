//! `fm-archive-state-files-loose-object-bloat-no-pack` — P3 detect-only.
//!
//! **Subsystem**: archive_state_files.
//!
//! ## What's broken
//!
//! Per-project git archives accumulate **loose objects** (one file
//! per object under `.git/objects/<XX>/<hash>`) every time the
//! commit coalescer flushes. Healthy archives are periodically
//! packed via `git gc` / `git maintenance run --task=gc` /
//! `am doctor pack-archive`, which collapses thousands of loose
//! objects into a single pack file. When packing falls behind,
//! the archive bloats:
//!
//! - **Inode pressure**: a project with 100K messages can have
//!   200K+ loose objects, hitting per-fs inode caps on small
//!   filesystems and slowing `readdir` on every commit.
//! - **Slow `am robot status` / `git log`**: every object lookup
//!   walks the 256-subdir loose tree before consulting pack
//!   indexes.
//! - **Disk amplification**: loose objects are zlib-compressed
//!   individually; packs use delta compression across related
//!   objects and are typically 5-10x smaller.
//!
//! P3 because this is performance, not correctness — `git`
//! still works correctly, just slowly. Operators rarely notice
//! until commit latency exceeds ~500ms or backups take an
//! order-of-magnitude longer.
//!
//! ## Detection (pure)
//!
//! For each project archive dir in `archive_roots`:
//!
//! 1. Find the per-project `.git/objects` dir.
//! 2. Enumerate the 256 hex subdirs (`00`..`ff`).
//! 3. For each subdir, count files + sum their sizes.
//!    (`packs/` is owned by git's pack machinery — we read it
//!    only to detect "no packs at all" as a corroborating
//!    signal, not as a count target.)
//! 4. Emit a finding for any project where:
//!    - `loose_count >= LOOSE_COUNT_THRESHOLD` (default 2048), OR
//!    - `loose_bytes >= LOOSE_BYTES_THRESHOLD` (default 256 MiB), OR
//!    - `loose_count > 0` AND `packs_count == 0` (a project that
//!      has never been packed at all is a clear maintenance gap).
//!
//! Returns one aggregated finding listing each affected project
//! with per-project stats. Operators triage by inspecting the
//! `summary.totals` field, then run `am doctor pack-archive`.
//!
//! ## Fix
//!
//! **Detect-only.** Packing is reversibly safe (no data deleted —
//! loose objects are removed only after the pack file is fsynced),
//! but it requires a multi-process lock (the chain runner +
//! background commit coalescer must not race the pack), Op::WriteFile
//! of new pack files + Op::Rename of the index, and a verification
//! step. `am doctor pack-archive` already implements this end-to-end.
//! Manual remediation routes operators there.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-archive-state-files-loose-object-bloat-no-pack";
const FM_SEVERITY: &str = "P3";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Default loose-object count threshold (per project). 2048 is
/// the lower bound where `readdir` on the 256-subdir tree
/// noticeably slows down `git log` / `git status`. Tunable via
/// `DetectInputs.loose_count_threshold_override`.
pub const DEFAULT_LOOSE_COUNT_THRESHOLD: usize = 2048;

/// Default loose-byte threshold (per project). 256 MiB ~= the
/// point where loose-object overhead exceeds typical pack-file
/// compression headroom (~ 5-10x). Tunable.
pub const DEFAULT_LOOSE_BYTES_THRESHOLD: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct BloatedRepoEntry {
    pub repo_path: PathBuf,
    pub loose_count: usize,
    pub loose_bytes: u64,
    pub packs_count: usize,
    /// Reason this project tripped — populated for operator
    /// triage (operators with a tight time budget can filter
    /// `entries[].reason == "no_packs_at_all"` first since
    /// those are the worst-case offenders).
    pub reason: BloatReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BloatReason {
    /// `loose_count` exceeded the count threshold.
    CountThreshold,
    /// `loose_bytes` exceeded the byte threshold.
    BytesThreshold,
    /// No pack files at all AND at least one loose object — a
    /// project that has NEVER been packed.
    NoPacksAtAll,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveLooseObjectBloatFinding {
    pub entries: Vec<BloatedRepoEntry>,
    pub loose_count_threshold: usize,
    pub loose_bytes_threshold: u64,
}

impl ArchiveLooseObjectBloatFinding {
    pub fn total_loose_count(&self) -> usize {
        self.entries.iter().map(|e| e.loose_count).sum()
    }
    pub fn total_loose_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.loose_bytes).sum()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} archive project(s) have loose-object bloat: {} loose objects, {} bytes total (run `am doctor pack-archive`)",
            self.entries.len(),
            self.total_loose_count(),
            self.total_loose_bytes(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "entries": self.entries,
                "summary": {
                    "total_loose_count": self.total_loose_count(),
                    "total_loose_bytes": self.total_loose_bytes(),
                    "loose_count_threshold": self.loose_count_threshold,
                    "loose_bytes_threshold": self.loose_bytes_threshold,
                },
                "manual_remediation": {
                    "steps": [
                        "Run `am doctor pack-archive` to pack every project archive (it already locks against the commit coalescer and verifies pack integrity before removing loose objects).",
                        "For a single project: `am doctor pack-archive --project <abs-path>` (faster than the whole-storage sweep).",
                        "Verify post-pack: re-run `am doctor fix --only fm-archive-state-files-loose-object-bloat-no-pack --list`; loose_count should drop below the threshold (typically to under 200).",
                    ],
                    "warning": "P3 performance — `git` still works correctly, just slowly. Pack when commit latency exceeds ~500ms or backups take noticeably longer.",
                    "common_causes": [
                        "Commit coalescer has been flushing into a project that's never been packed (the `no_packs_at_all` reason).",
                        "Periodic `git maintenance run` cron / systemd timer is disabled or has been failing silently.",
                        "Recent message backfill (e.g., via `am doctor reconstruct`) wrote many loose objects in one burst — pack reduces them to a single pack file.",
                        "Filesystem doesn't support hardlinks (some FUSE mounts), so `git gc` can't reclaim space efficiently — manual `git repack -a -d -f` may be needed.",
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
    /// Override loose-object count threshold. `None` uses
    /// `DEFAULT_LOOSE_COUNT_THRESHOLD`.
    pub loose_count_threshold_override: Option<usize>,
    /// Override loose-bytes threshold. `None` uses
    /// `DEFAULT_LOOSE_BYTES_THRESHOLD`.
    pub loose_bytes_threshold_override: Option<u64>,
}

/// Detector. PURE w.r.t. the supplied `archive_roots`.
///
/// Each entry in `archive_roots` is a per-project archive dir
/// (i.e., `<storage_root>/projects/<slug>/`) with a `.git/`
/// subdir. Non-git entries are silently skipped.
pub fn detect(
    archive_roots: &[PathBuf],
    inputs: &DetectInputs,
) -> Vec<ArchiveLooseObjectBloatFinding> {
    let loose_count_threshold = inputs
        .loose_count_threshold_override
        .unwrap_or(DEFAULT_LOOSE_COUNT_THRESHOLD);
    let loose_bytes_threshold = inputs
        .loose_bytes_threshold_override
        .unwrap_or(DEFAULT_LOOSE_BYTES_THRESHOLD);

    let mut entries: Vec<BloatedRepoEntry> = Vec::new();
    for repo_path in archive_roots {
        let Some(entry) = inspect_one_repo(repo_path, loose_count_threshold, loose_bytes_threshold)
        else {
            continue;
        };
        entries.push(entry);
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![ArchiveLooseObjectBloatFinding {
        entries,
        loose_count_threshold,
        loose_bytes_threshold,
    }]
}

fn inspect_one_repo(
    repo_path: &Path,
    loose_count_threshold: usize,
    loose_bytes_threshold: u64,
) -> Option<BloatedRepoEntry> {
    let objects_dir = repo_path.join(".git").join("objects");
    if !objects_dir.is_dir() {
        return None;
    }
    let (loose_count, loose_bytes) = count_loose_objects(&objects_dir);
    let packs_count = count_pack_files(&objects_dir);
    let reason = if loose_count > 0 && packs_count == 0 {
        BloatReason::NoPacksAtAll
    } else if loose_count >= loose_count_threshold {
        BloatReason::CountThreshold
    } else if loose_bytes >= loose_bytes_threshold {
        BloatReason::BytesThreshold
    } else {
        return None;
    };
    Some(BloatedRepoEntry {
        repo_path: repo_path.to_path_buf(),
        loose_count,
        loose_bytes,
        packs_count,
        reason,
    })
}

fn count_loose_objects(objects_dir: &Path) -> (usize, u64) {
    let mut total_count: usize = 0;
    let mut total_bytes: u64 = 0;
    let Ok(rd) = std::fs::read_dir(objects_dir) else {
        return (0, 0);
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        // Loose-object subdirs are 2-hex-digit names `00`..`ff`.
        // Skip `pack`, `info`, anything else.
        if name_str.len() != 2 || !name_str.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(sub_rd) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for obj in sub_rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(obj.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            total_count += 1;
            total_bytes = total_bytes.saturating_add(meta.len());
        }
    }
    (total_count, total_bytes)
}

fn count_pack_files(objects_dir: &Path) -> usize {
    let pack_dir = objects_dir.join("pack");
    let Ok(rd) = std::fs::read_dir(&pack_dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            if !e.file_type().is_ok_and(|ty| ty.is_file()) {
                return false;
            }
            let name = e.file_name();
            let n = name.to_string_lossy().to_string();
            // `.pack` files are the actual pack contents. `.idx`
            // files index them; we count packs, not indexes.
            n.ends_with(".pack")
        })
        .count()
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &ArchiveLooseObjectBloatFinding,
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

    fn make_repo_with_loose_objects(
        td: &TempDir,
        slug: &str,
        loose_count: usize,
        each_byte_size: usize,
    ) -> PathBuf {
        let repo = td.path().join(slug);
        let objects = repo.join(".git").join("objects");
        fs::create_dir_all(&objects).unwrap();
        // Plant `loose_count` loose objects spread across subdirs.
        // Each gets `each_byte_size` bytes.
        for i in 0..loose_count {
            let xx = format!("{:02x}", (i % 256) as u8);
            let sub = objects.join(&xx);
            fs::create_dir_all(&sub).unwrap();
            let blob_name = format!("{:062x}", i); // 62 hex = remaining part of a 40-byte sha1
            fs::write(sub.join(blob_name), vec![0u8; each_byte_size]).unwrap();
        }
        repo
    }

    fn add_pack_file(repo: &Path) {
        let pack = repo.join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack).unwrap();
        // Create a sentinel `.pack` file. We don't need real git
        // content — the detector only counts `*.pack` filenames.
        fs::write(pack.join("pack-deadbeef.pack"), b"FAKE PACK\n").unwrap();
        fs::write(pack.join("pack-deadbeef.idx"), b"FAKE IDX\n").unwrap();
    }

    /// **NEGATIVE TEST FIRST**: empty input → no finding.
    #[test]
    fn detector_returns_empty_for_no_candidates() {
        assert!(detect(&[], &DetectInputs::default()).is_empty());
    }

    /// **NEGATIVE**: a non-git directory is silently skipped.
    #[test]
    fn detector_skips_non_git_directory() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("not-a-repo");
        fs::create_dir_all(&dir).unwrap();
        let findings = detect(&[dir], &DetectInputs::default());
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: a healthy repo with few loose objects + a pack
    /// → no finding.
    #[test]
    fn detector_returns_empty_for_healthy_repo() {
        let td = TempDir::new().unwrap();
        let repo = make_repo_with_loose_objects(&td, "healthy", 5, 100);
        add_pack_file(&repo);
        let findings = detect(&[repo], &DetectInputs::default());
        assert!(
            findings.is_empty(),
            "healthy repo with few loose + a pack must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_count_threshold_bloat() {
        let td = TempDir::new().unwrap();
        // Plant 50 loose objects, override count threshold to 10.
        let repo = make_repo_with_loose_objects(&td, "bloated", 50, 100);
        add_pack_file(&repo);
        let inputs = DetectInputs {
            loose_count_threshold_override: Some(10),
            loose_bytes_threshold_override: None,
        };
        let findings = detect(&[repo], &inputs);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].loose_count, 50);
        assert_eq!(f.entries[0].reason, BloatReason::CountThreshold);
        assert_eq!(f.entries[0].packs_count, 1);
    }

    #[test]
    fn detector_flags_bytes_threshold_bloat() {
        let td = TempDir::new().unwrap();
        // 20 loose objects × 1KB each = 20 KB; bytes threshold 10 KB.
        let repo = make_repo_with_loose_objects(&td, "fat", 20, 1024);
        add_pack_file(&repo);
        let inputs = DetectInputs {
            loose_count_threshold_override: Some(100), // count under cap
            loose_bytes_threshold_override: Some(10 * 1024),
        };
        let findings = detect(&[repo], &inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].reason, BloatReason::BytesThreshold);
    }

    #[test]
    fn detector_flags_no_packs_at_all_reason() {
        let td = TempDir::new().unwrap();
        // Only 5 loose objects (well under count cap) and ZERO
        // packs — must still flag as `no_packs_at_all`.
        let repo = make_repo_with_loose_objects(&td, "never-packed", 5, 100);
        let findings = detect(&[repo], &DetectInputs::default());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].reason, BloatReason::NoPacksAtAll);
        assert_eq!(findings[0].entries[0].packs_count, 0);
    }

    #[test]
    fn detector_aggregates_multiple_repos_into_one_finding() {
        let td = TempDir::new().unwrap();
        let repo_a = make_repo_with_loose_objects(&td, "a", 50, 100);
        add_pack_file(&repo_a);
        let repo_b = make_repo_with_loose_objects(&td, "b", 30, 100);
        add_pack_file(&repo_b);
        let inputs = DetectInputs {
            loose_count_threshold_override: Some(10),
            loose_bytes_threshold_override: None,
        };
        let findings = detect(&[repo_a, repo_b], &inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 2);
        assert_eq!(findings[0].total_loose_count(), 80);
    }

    #[test]
    fn count_loose_objects_skips_non_hex_subdirs() {
        let td = TempDir::new().unwrap();
        let objects = td.path().join(".git").join("objects");
        fs::create_dir_all(objects.join("ab")).unwrap();
        fs::write(objects.join("ab").join("blob1"), b"x").unwrap();
        // `pack` and `info` are valid git subdirs but NOT loose
        // object subdirs.
        fs::create_dir_all(objects.join("pack")).unwrap();
        fs::write(objects.join("pack").join("p.pack"), b"x").unwrap();
        fs::create_dir_all(objects.join("info")).unwrap();
        fs::write(objects.join("info").join("packs"), b"x").unwrap();
        // A 3-hex-letter dir must also be skipped.
        fs::create_dir_all(objects.join("abc")).unwrap();
        fs::write(objects.join("abc").join("x"), b"x").unwrap();
        let (count, _) = count_loose_objects(&objects);
        assert_eq!(count, 1, "must only count the `ab/blob1`");
    }

    #[cfg(unix)]
    #[test]
    fn count_loose_objects_does_not_follow_symlinked_objects() {
        use std::os::unix::fs::symlink;

        let td = TempDir::new().unwrap();
        let objects = td.path().join(".git").join("objects");
        fs::create_dir_all(objects.join("ab")).unwrap();
        let outside = td.path().join("outside-object");
        fs::write(&outside, b"x").unwrap();
        symlink(&outside, objects.join("ab").join("linked-object")).unwrap();

        let (count, _) = count_loose_objects(&objects);
        assert_eq!(
            count, 0,
            "git loose-object counting must not follow symlinked entries"
        );
    }

    #[test]
    fn count_pack_files_ignores_pack_named_directories() {
        let td = TempDir::new().unwrap();
        let pack = td.path().join(".git").join("objects").join("pack");
        fs::create_dir_all(pack.join("fake.pack")).unwrap();
        fs::write(pack.join("real.pack"), b"PACK\n").unwrap();
        fs::write(pack.join("real.idx"), b"IDX\n").unwrap();

        assert_eq!(
            count_pack_files(&td.path().join(".git").join("objects")),
            1,
            "only regular .pack files should count as git packs"
        );
    }

    #[test]
    fn finding_serializes_with_reason_strings_and_summary() {
        let f = ArchiveLooseObjectBloatFinding {
            entries: vec![BloatedRepoEntry {
                repo_path: "/tmp/proj-a".into(),
                loose_count: 5000,
                loose_bytes: 50_000_000,
                packs_count: 0,
                reason: BloatReason::NoPacksAtAll,
            }],
            loose_count_threshold: DEFAULT_LOOSE_COUNT_THRESHOLD,
            loose_bytes_threshold: DEFAULT_LOOSE_BYTES_THRESHOLD,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"no_packs_at_all\""));
        assert!(s.contains("\"total_loose_count\":5000"));
        assert!(s.contains("am doctor pack-archive"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
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
        let finding = ArchiveLooseObjectBloatFinding {
            entries: Vec::new(),
            loose_count_threshold: DEFAULT_LOOSE_COUNT_THRESHOLD,
            loose_bytes_threshold: DEFAULT_LOOSE_BYTES_THRESHOLD,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
