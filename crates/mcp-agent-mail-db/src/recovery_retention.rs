//! Bounded retention for doctor recovery debris (br-mudrv).
//!
//! `am doctor repair`/`reconstruct` and the startup self-heal capture a
//! forensic bundle + quarantine the bad DB on every recovery event, with NO
//! retention. Across repeated corruption events this grows without bound (a
//! prod box reached ~19 GB of `doctor/` dumps before startup wedged). This
//! module bounds that growth.
//!
//! Two surfaces consume it:
//! - the background integrity-guard maintenance sweep, which OBSERVES + ALERTS
//!   (it never deletes — the forensic-bundle manifest declares
//!   `automatic_deletion: false` and RULE 1 forbids it), and
//! - the operator-explicit `am doctor reclaim` verb, which CONSOLIDATES the
//!   excess into one reversible `doctor/reclaimable/<ts>/` directory (a
//!   rename, never a delete — matching the doctor's quarantine philosophy).
//!
//! The selection logic ([`select_recovery_debris_to_reclaim`]) is PURE so it is
//! exhaustively unit-testable; the filesystem enumeration and the move are
//! thin IO wrappers around it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Which kind of recovery debris an artifact is. Retention is applied
/// independently per category so a burst of one kind cannot evict the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebrisCategory {
    /// A `doctor/forensics/<db_family>/<cmd>-<ts>/` bundle directory (each holds
    /// a full copy of the DB + sidecars, so they are large).
    ForensicBundle,
    /// A quarantined corrupt DB / sidecar sibling of the live database, e.g.
    /// `storage.sqlite3.corrupt-<ts>` or `storage.sqlite3-wal.reconstruct-failed-<ts>`.
    CorruptQuarantine,
    /// A startup-time WAL/SHM sidecar snapshot next to the live database, e.g.
    /// `storage.sqlite3-wal.startup-precheckpoint-<ts>` (copied before the
    /// startup `wal_checkpoint(TRUNCATE)`) or `*.startup-quarantine-<ts>`.
    /// These are written on every startup that finds a non-empty stale WAL and
    /// previously had NO retention at all, so they accumulate without bound
    /// (GH#185 reported 66 of them dating back weeks).
    SidecarSnapshot,
}

impl DebrisCategory {
    /// Stable, lower-case label for logs / JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForensicBundle => "forensic_bundle",
            Self::CorruptQuarantine => "corrupt_quarantine",
            Self::SidecarSnapshot => "sidecar_snapshot",
        }
    }
}

/// One reclaimable artifact: a forensic bundle directory or a quarantine file.
#[derive(Debug, Clone)]
pub struct DebrisArtifact {
    pub path: PathBuf,
    /// Total bytes on disk (recursive for bundle directories).
    pub bytes: u64,
    /// Last-modified time in microseconds since the Unix epoch (`0` if unknown).
    pub modified_us: i64,
    pub category: DebrisCategory,
}

/// Retention policy: within each category keep the `keep_min` NEWEST artifacts
/// plus anything younger than `max_age_secs`; everything else is reclaimable.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub keep_min: usize,
    pub max_age_secs: u64,
}

/// The result of applying a [`RetentionPolicy`] to a set of artifacts.
#[derive(Debug, Clone, Default)]
pub struct ReclaimPlan {
    /// Artifacts selected for reclamation, oldest-first.
    pub prune: Vec<DebrisArtifact>,
    pub kept_count: usize,
    pub total_count: usize,
    pub reclaimable_bytes: u64,
    pub total_bytes: u64,
}

impl ReclaimPlan {
    /// Whether any artifact is reclaimable under the policy.
    #[must_use]
    pub fn has_reclaimable(&self) -> bool {
        !self.prune.is_empty()
    }
}

/// PURE: choose which debris artifacts to reclaim under `policy`.
///
/// Within each [`DebrisCategory`], the `keep_min` newest artifacts are always
/// retained, and any artifact younger than `max_age_secs` is always retained.
/// Only artifacts that are BOTH older than `max_age_secs` AND beyond the
/// `keep_min` newest are reclaimed. A negative/zero/unknown `modified_us` is
/// treated as very old (eligible once past `keep_min`). The returned prune list
/// is ordered oldest-first for a stable reclaim order.
#[must_use]
pub fn select_recovery_debris_to_reclaim(
    mut artifacts: Vec<DebrisArtifact>,
    policy: RetentionPolicy,
    now_us: i64,
) -> ReclaimPlan {
    let total_count = artifacts.len();
    let total_bytes = artifacts
        .iter()
        .map(|a| a.bytes)
        .fold(0_u64, u64::saturating_add);
    let max_age_us = i128::from(policy.max_age_secs).saturating_mul(1_000_000);

    // Newest-first so `keep_min` protects the most recent artifacts. Stable so
    // equal-mtime ties keep a deterministic order.
    artifacts.sort_by_key(|art| std::cmp::Reverse(art.modified_us));

    let mut rank_per_category: HashMap<DebrisCategory, usize> = HashMap::new();
    let mut prune = Vec::new();
    for art in artifacts {
        let rank = rank_per_category.entry(art.category).or_insert(0);
        let within_keep_min = *rank < policy.keep_min;
        *rank += 1;
        if within_keep_min {
            continue;
        }
        let age_us = i128::from(now_us).saturating_sub(i128::from(art.modified_us));
        let young = age_us < max_age_us;
        if young {
            continue;
        }
        prune.push(art);
    }

    prune.sort_by_key(|art| art.modified_us);
    let reclaimable_bytes = prune
        .iter()
        .map(|a| a.bytes)
        .fold(0_u64, u64::saturating_add);
    let kept_count = total_count - prune.len();
    ReclaimPlan {
        prune,
        kept_count,
        total_count,
        reclaimable_bytes,
        total_bytes,
    }
}

/// Enumerate all recovery debris under `storage_root` / next to `db_path`.
///
/// Combines forensic bundles (`doctor/forensics/.../`) with quarantine siblings
/// (`<db>.corrupt-*`, `<db>.reconstruct-failed-*`, `<db>.archive-reconcile-restore-*`)
/// and startup sidecar snapshots (`<db>-wal.startup-precheckpoint-*`,
/// `<db>-shm.startup-precheckpoint-*`, `<db>*.startup-quarantine-*`).
#[must_use]
pub fn enumerate_recovery_debris(storage_root: &Path, db_path: &Path) -> Vec<DebrisArtifact> {
    let mut out = enumerate_forensic_bundles(storage_root);
    out.extend(enumerate_corrupt_quarantines(db_path));
    out
}

/// Enumerate forensic bundle directories under `<storage_root>/doctor/forensics/`.
#[must_use]
pub fn enumerate_forensic_bundles(storage_root: &Path) -> Vec<DebrisArtifact> {
    let root = storage_root.join("doctor").join("forensics");
    let mut out = Vec::new();
    let Ok(families) = std::fs::read_dir(&root) else {
        return out;
    };
    for family in families.flatten() {
        let family_path = family.path();
        // Each db-family directory holds the per-command bundle directories.
        let Ok(meta) = std::fs::symlink_metadata(&family_path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let Ok(bundles) = std::fs::read_dir(&family_path) else {
            continue;
        };
        for bundle in bundles.flatten() {
            let bundle_path = bundle.path();
            let Ok(meta) = std::fs::symlink_metadata(&bundle_path) else {
                continue;
            };
            if !meta.file_type().is_dir() {
                continue;
            }
            out.push(DebrisArtifact {
                bytes: dir_size_bytes(&bundle_path),
                modified_us: mtime_us(&meta),
                path: bundle_path,
                category: DebrisCategory::ForensicBundle,
            });
        }
    }
    out
}

/// Enumerate quarantined corrupt-DB siblings next to `db_path`.
#[must_use]
pub fn enumerate_corrupt_quarantines(db_path: &Path) -> Vec<DebrisArtifact> {
    let mut out = Vec::new();
    let Some(parent) = db_path.parent() else {
        return out;
    };
    let Some(db_name) = db_path.file_name().and_then(|n| n.to_str()) else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if !name.starts_with(db_name) {
            continue;
        }
        let category = if is_quarantine_name(name) {
            DebrisCategory::CorruptQuarantine
        } else if is_sidecar_snapshot_name(name) {
            DebrisCategory::SidecarSnapshot
        } else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        out.push(DebrisArtifact {
            bytes: meta.len(),
            modified_us: mtime_us(&meta),
            path,
            category,
        });
    }
    out
}

/// Whether a filename is a recovery quarantine (corrupt / reconstruct-failed /
/// archive-reconcile-restore), as opposed to the live DB, a `.bak`, or a live
/// `-wal`/`-shm` sidecar.
#[must_use]
pub fn is_quarantine_name(name: &str) -> bool {
    name.contains(".corrupt-")
        || name.contains(".reconstruct-failed-")
        || name.contains(".archive-reconcile-restore-")
}

/// Whether a filename is a startup-time WAL/SHM sidecar snapshot
/// (`*.startup-precheckpoint-<ts>` / `*.startup-quarantine-<ts>`), as opposed
/// to the live DB, a `.bak`, or a live `-wal`/`-shm` sidecar. These snapshots
/// are forensic copies taken before the startup checkpoint and are safe to
/// reclaim under the same bounded-retention policy as quarantines (GH#185).
#[must_use]
pub fn is_sidecar_snapshot_name(name: &str) -> bool {
    name.contains(".startup-precheckpoint-") || name.contains(".startup-quarantine-")
}

/// Outcome of [`consolidate_debris`].
#[derive(Debug, Clone, Default)]
pub struct ReclaimOutcome {
    pub moved: usize,
    pub moved_bytes: u64,
    /// `(path, error_message)` for artifacts that could not be moved.
    pub failures: Vec<(PathBuf, String)>,
}

/// Consolidate (MOVE — never delete) the planned debris into `dest_dir`.
///
/// Per RULE 1 and the forensic-bundle manifest's no-automatic-deletion
/// contract, this never removes data; it relocates each artifact under one
/// operator-reclaimable directory so disk is freed only by an explicit later
/// `rm` the operator chooses to run. Same-filesystem renames are atomic and
/// cheap.
pub fn consolidate_debris(plan: &ReclaimPlan, dest_dir: &Path) -> std::io::Result<ReclaimOutcome> {
    if plan.prune.is_empty() {
        return Ok(ReclaimOutcome::default());
    }
    std::fs::create_dir_all(dest_dir)?;
    let mut outcome = ReclaimOutcome::default();
    for art in &plan.prune {
        let Some(name) = art.path.file_name() else {
            outcome
                .failures
                .push((art.path.clone(), "artifact has no file name".to_string()));
            continue;
        };
        let dest = unique_dest_path(dest_dir, name);
        match std::fs::rename(&art.path, &dest) {
            Ok(()) => {
                outcome.moved += 1;
                outcome.moved_bytes = outcome.moved_bytes.saturating_add(art.bytes);
            }
            Err(err) => outcome.failures.push((art.path.clone(), err.to_string())),
        }
    }
    Ok(outcome)
}

/// Pick a destination path under `dest_dir` that does not collide with an
/// existing entry (suffix `.1`, `.2`, ... on conflict).
fn unique_dest_path(dest_dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dest_dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for n in 1..u32::MAX {
        let mut alt = name.to_os_string();
        alt.push(format!(".{n}"));
        let candidate = dest_dir.join(&alt);
        if !candidate.exists() {
            return candidate;
        }
    }
    dest_dir.join(name)
}

/// Recursive on-disk byte total for a directory, without following symlinks
/// (so a symlinked entry contributes nothing and cannot cause a cycle).
#[must_use]
fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn mtime_us(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_US: i64 = 3_600 * 1_000_000;
    const DAY_US: i64 = 24 * HOUR_US;

    fn art(name: &str, bytes: u64, modified_us: i64, category: DebrisCategory) -> DebrisArtifact {
        DebrisArtifact {
            path: PathBuf::from(name),
            bytes,
            modified_us,
            category,
        }
    }

    #[test]
    fn keeps_keep_min_newest_even_when_old() {
        // 4 bundles, all older than max_age; keep_min=2 must retain the 2 newest.
        let now = 100 * DAY_US;
        let artifacts = vec![
            art("b1", 10, now - 40 * DAY_US, DebrisCategory::ForensicBundle),
            art("b2", 10, now - 30 * DAY_US, DebrisCategory::ForensicBundle),
            art("b3", 10, now - 20 * DAY_US, DebrisCategory::ForensicBundle),
            art("b4", 10, now - 10 * DAY_US, DebrisCategory::ForensicBundle),
        ];
        let policy = RetentionPolicy {
            keep_min: 2,
            max_age_secs: 14 * 24 * 3_600,
        };
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // b3 + b4 are the 2 newest → kept; b1 + b2 are old + beyond keep_min → pruned.
        assert_eq!(plan.prune.len(), 2);
        assert_eq!(plan.kept_count, 2);
        assert_eq!(plan.reclaimable_bytes, 20);
        assert_eq!(plan.total_bytes, 40);
        // oldest-first ordering
        assert_eq!(plan.prune[0].path, PathBuf::from("b1"));
        assert_eq!(plan.prune[1].path, PathBuf::from("b2"));
    }

    #[test]
    fn keeps_young_artifacts_beyond_keep_min() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 14 * 24 * 3_600,
        };
        let artifacts = vec![
            art(
                "old",
                10,
                now - 30 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "young1",
                10,
                now - DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "young2",
                10,
                now - 2 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // keep_min=1 keeps young1 (newest); young2 is <14d → kept by age; old is pruned.
        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("old"));
    }

    #[test]
    fn retention_is_per_category() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 0, // nothing is "young"; only keep_min protects
        };
        let artifacts = vec![
            art(
                "f_old",
                5,
                now - 30 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "f_new",
                5,
                now - 10 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "q_old",
                7,
                now - 30 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "q_new",
                7,
                now - 10 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // keep_min=1 per category keeps f_new + q_new; prunes f_old + q_old.
        let pruned: Vec<_> = plan
            .prune
            .iter()
            .map(|a| a.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plan.prune.len(), 2);
        assert!(pruned.contains(&"f_old".to_string()));
        assert!(pruned.contains(&"q_old".to_string()));
        assert_eq!(plan.reclaimable_bytes, 12);
    }

    #[test]
    fn empty_input_is_noop() {
        let plan = select_recovery_debris_to_reclaim(
            Vec::new(),
            RetentionPolicy {
                keep_min: 5,
                max_age_secs: 0,
            },
            0,
        );
        assert!(!plan.has_reclaimable());
        assert_eq!(plan.total_count, 0);
    }

    #[test]
    fn unknown_mtime_is_treated_as_old() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 14 * 24 * 3_600,
        };
        let artifacts = vec![
            art(
                "known_new",
                10,
                now - DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art("unknown", 10, 0, DebrisCategory::ForensicBundle),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // known_new kept by keep_min; unknown (mtime 0 → very old) pruned.
        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("unknown"));
    }

    #[test]
    fn quarantine_name_classification() {
        assert!(is_quarantine_name(
            "storage.sqlite3.corrupt-20260618_145230_042"
        ));
        assert!(is_quarantine_name(
            "storage.sqlite3-wal.corrupt-20260618_145230_042"
        ));
        assert!(is_quarantine_name(
            "storage.sqlite3.reconstruct-failed-20260618_145230_042"
        ));
        assert!(is_quarantine_name(
            "storage.sqlite3.archive-reconcile-restore-20260618_145230_042"
        ));
        // Not quarantines: live DB, backup, live sidecars.
        assert!(!is_quarantine_name("storage.sqlite3"));
        assert!(!is_quarantine_name("storage.sqlite3.bak"));
        assert!(!is_quarantine_name("storage.sqlite3-wal"));
        assert!(!is_quarantine_name("storage.sqlite3-shm"));
        assert!(!is_quarantine_name("storage.sqlite3.bak.meta.json"));
    }

    #[test]
    fn sidecar_snapshot_name_classification() {
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-wal.startup-precheckpoint-20260625_120000_000"
        ));
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-shm.startup-precheckpoint-20260625_120000_000"
        ));
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-wal.startup-quarantine-20260625_120000_000"
        ));
        // Live DB / backup / live sidecars are NOT snapshots.
        assert!(!is_sidecar_snapshot_name("storage.sqlite3"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3.bak"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3-wal"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3-shm"));
        // Quarantines are a different category.
        assert!(!is_sidecar_snapshot_name(
            "storage.sqlite3.corrupt-20260618_145230_042"
        ));
    }

    #[test]
    fn enumerates_startup_precheckpoint_snapshots_as_sidecar_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();
        std::fs::write(storage_root.join("storage.sqlite3-wal"), b"live-wal").unwrap();
        for ts in [
            "20260625_120000_000",
            "20260626_120000_000",
            "20260627_120000_000",
        ] {
            std::fs::write(
                storage_root.join(format!("storage.sqlite3-wal.startup-precheckpoint-{ts}")),
                vec![0_u8; 100],
            )
            .unwrap();
        }
        std::fs::write(
            storage_root.join("storage.sqlite3-shm.startup-precheckpoint-20260625_120000_000"),
            vec![0_u8; 50],
        )
        .unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        let snapshots: Vec<_> = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::SidecarSnapshot)
            .collect();
        assert_eq!(snapshots.len(), 4, "debris: {debris:?}");
        // The live DB and live WAL sidecar are never enumerated as debris.
        assert!(debris.iter().all(|a| a.path != db_path));
        assert!(
            debris
                .iter()
                .all(|a| a.path != storage_root.join("storage.sqlite3-wal"))
        );

        // All are reclaimable under keep_min=0/max_age=0 and consolidate cleanly.
        let plan = select_recovery_debris_to_reclaim(
            debris,
            RetentionPolicy {
                keep_min: 0,
                max_age_secs: 0,
            },
            i64::MAX,
        );
        assert_eq!(plan.prune.len(), 4);
        let dest = storage_root.join("doctor").join("reclaimable").join("run");
        let outcome = consolidate_debris(&plan, &dest).unwrap();
        assert_eq!(outcome.moved, 4, "failures: {:?}", outcome.failures);
        assert!(
            !storage_root
                .join("storage.sqlite3-wal.startup-precheckpoint-20260625_120000_000")
                .exists()
        );
    }

    #[test]
    fn enumerate_and_consolidate_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();

        // Two forensic bundles, each with a nested file.
        let forensics = storage_root
            .join("doctor")
            .join("forensics")
            .join("storage.sqlite3");
        for bundle in ["repair-20260101_000000_000", "repair-20260102_000000_000"] {
            let b = forensics.join(bundle).join("sqlite");
            std::fs::create_dir_all(&b).unwrap();
            std::fs::write(b.join("storage.sqlite3"), vec![0_u8; 1000]).unwrap();
        }
        // Two quarantine files + a .bak that must be ignored.
        std::fs::write(
            storage_root.join("storage.sqlite3.corrupt-20260101_000000_000"),
            vec![0_u8; 500],
        )
        .unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3.corrupt-20260102_000000_000"),
            vec![0_u8; 500],
        )
        .unwrap();
        std::fs::write(storage_root.join("storage.sqlite3.bak"), b"backup").unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        // 2 bundles + 2 quarantines (the .bak and live DB are excluded).
        assert_eq!(debris.len(), 4, "debris: {debris:?}");
        let bundles = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::ForensicBundle)
            .count();
        let quarantines = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::CorruptQuarantine)
            .count();
        assert_eq!(bundles, 2);
        assert_eq!(quarantines, 2);
        // each bundle includes the 1000-byte nested file
        assert!(
            debris
                .iter()
                .filter(|a| a.category == DebrisCategory::ForensicBundle)
                .all(|a| a.bytes >= 1000)
        );

        // Reclaim everything (keep_min=0, max_age=0) by consolidating into a dest.
        let plan = select_recovery_debris_to_reclaim(
            debris,
            RetentionPolicy {
                keep_min: 0,
                max_age_secs: 0,
            },
            i64::MAX,
        );
        assert_eq!(plan.prune.len(), 4);
        let dest = storage_root
            .join("doctor")
            .join("reclaimable")
            .join("run-x");
        let outcome = consolidate_debris(&plan, &dest).unwrap();
        assert_eq!(outcome.moved, 4, "failures: {:?}", outcome.failures);
        assert!(outcome.failures.is_empty());
        // The live DB and .bak survive; quarantines are gone from the DB dir.
        assert!(db_path.exists());
        assert!(storage_root.join("storage.sqlite3.bak").exists());
        assert!(
            !storage_root
                .join("storage.sqlite3.corrupt-20260101_000000_000")
                .exists()
        );
        // Consolidated copies landed in the dest.
        assert!(
            dest.join("storage.sqlite3.corrupt-20260101_000000_000")
                .exists()
        );
    }
}
