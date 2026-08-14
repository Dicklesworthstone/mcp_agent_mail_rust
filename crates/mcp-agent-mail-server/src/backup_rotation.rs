//! Bounded retention for storage-root backup detritus.
//!
//! Background: every SQLite corruption, reconstruction, or archive-reconcile
//! cycle creates a dated backup file (e.g., `storage.sqlite3.corrupt-20260419_...`).
//! Without rotation these accumulate forever. The 2026-04-19 incident had
//! 25 `.corrupt-*`, 17 `.reconstruct-failed-*`, and 40+ `.archive-reconcile-*`
//! files totaling ~1.3 GB in a single storage_root.
//!
//! This module keeps the N most recent of each *kind* and quarantines the
//! rest by moving them into `doctor/reclaimable/rotation-<ts>/` inside the
//! storage root, where an operator (or `am doctor`) can inspect and reclaim
//! them later. Nothing is hard-deleted unless the operator explicitly opts
//! back in via `AM_BACKUP_ROTATION_DELETE`. Kinds are classified by filename
//! suffix pattern so we never touch the live DB (`storage.sqlite3`), the
//! Codex sidecar DB, or unrelated files.
//!
//! Default `keep_per_kind = 3`; override via `AM_BACKUP_KEEP_COUNT`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{debug, info, warn};

/// How many of each kind to keep when rotating. Override via
/// `AM_BACKUP_KEEP_COUNT`. Floor of 1 (keep the most recent no matter what).
const DEFAULT_KEEP_PER_KIND: usize = 3;
const MIN_KEEP_PER_KIND: usize = 1;

// Lifted to the db crate (GH#210) so the MCP `health_check` retention block
// (tools crate, which cannot depend on this crate) and `am doctor health`
// consume the SAME classifier and inventory. Re-exported here so rotation
// call sites and downstream consumers keep compiling unchanged.
pub use mcp_agent_mail_db::recovery_retention::{
    BackupInventory, BackupInventoryArtifact, BackupKind, classify_backup_file,
    inspect_storage_backups,
};

/// Report returned by `rotate_storage_backups`. One entry per non-empty kind.
///
/// "Staged" means moved into the `doctor/reclaimable/rotation-<ts>/`
/// quarantine directory (the default); "deleted" means hard-removed, which
/// only happens behind the explicit `AM_BACKUP_ROTATION_DELETE` opt-in.
/// Staged bytes are *not* reclaimed disk — they still live in the storage
/// root until an operator reclaims them — so the fields say what actually
/// happened rather than claiming savings that didn't occur.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateReport {
    pub kept: usize,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
    pub per_kind: BTreeMap<&'static str, RotateKindSummary>,
}

impl RotateReport {
    /// Total files evicted from retention, regardless of whether they were
    /// staged into quarantine or hard-deleted under the opt-in.
    #[must_use]
    pub const fn evicted(&self) -> usize {
        self.staged.saturating_add(self.deleted)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateKindSummary {
    pub kept: usize,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
}

/// Resolve the rotation "keep count" — honors `AM_BACKUP_KEEP_COUNT` env
/// override; falls back to `DEFAULT_KEEP_PER_KIND`. Floor of `MIN_KEEP_PER_KIND`.
#[must_use]
pub fn resolved_keep_per_kind() -> usize {
    mcp_agent_mail_core::config::process_env_value("AM_BACKUP_KEEP_COUNT")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEEP_PER_KIND)
        .max(MIN_KEEP_PER_KIND)
}

/// Whether the operator opted back into legacy hard-delete rotation.
///
/// Read from `AM_BACKUP_ROTATION_DELETE`. Default (unset or falsy) stages
/// evicted backups under `doctor/reclaimable/` instead of deleting them, so
/// rotation never destroys data on its own.
///
/// Uses the same truthy vocabulary as `mcp_agent_mail_core`'s bool parsing.
#[must_use]
pub fn rotation_delete_opted_in() -> bool {
    mcp_agent_mail_core::config::process_env_value("AM_BACKUP_ROTATION_DELETE").is_some_and(|v| {
        matches!(
            v.trim().to_lowercase().as_str(),
            "1" | "true" | "t" | "yes" | "y"
        )
    })
}

/// Rotate backup files in `storage_root`, staging evictions for reclaim.
///
/// Keeps `keep_per_kind` newest of each kind and stages the rest into
/// `<storage_root>/doctor/reclaimable/rotation-<UTC ts>/` for operator
/// reclaim. With the explicit `AM_BACKUP_ROTATION_DELETE` opt-in the evicted
/// files are hard-deleted instead (the legacy behavior).
///
/// Non-backup files (live DB, Codex DB, projects/, search_index/, .git/,
/// etc.) are never touched. Rotation only applies to files classified as
/// backups. `storage.sqlite3.archive-reconcile-*` files are additionally
/// excluded even though they classify as backups — see the comment inside.
///
/// Returns a `RotateReport` with per-kind counts. Errors on individual
/// stage/delete operations are logged and counted as `kept` so partial
/// failures don't mask themselves.
pub fn rotate_storage_backups(
    storage_root: &Path,
    keep_per_kind: usize,
) -> std::io::Result<RotateReport> {
    let keep = keep_per_kind.max(MIN_KEEP_PER_KIND);
    let delete_opted_in = rotation_delete_opted_in();

    let mut report = RotateReport::default();
    let entries = match fs::read_dir(storage_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e),
    };

    // Group candidate files by kind so we can rotate each independently.
    let mut by_kind: BTreeMap<BackupKind, Vec<(PathBuf, SystemTime, u64)>> = BTreeMap::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(kind) = classify_backup_file(&name) else {
            continue;
        };
        // Single ownership: `storage.sqlite3.archive-reconcile-*` snapshots
        // are owned by the recovery-retention reclaim planner
        // (`mcp-agent-mail-db/src/recovery_retention.rs`), which stages them
        // with its own policy. If rotation also evicted them, a snapshot
        // could disappear before the planner stages it — so rotation leaves
        // this kind alone entirely.
        if kind == BackupKind::ArchiveReconcile {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        by_kind
            .entry(kind)
            .or_default()
            .push((entry.path(), mtime, meta.len()));
    }

    // Quarantine directory for this rotation pass. Created lazily on the
    // first staged file so a no-op rotation leaves no empty directories.
    let quarantine_dir = storage_root
        .join("doctor")
        .join("reclaimable")
        .join(format!(
            "rotation-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
    let mut quarantine_ready = false;

    for (kind, mut files) in by_kind {
        // Sort descending by mtime — oldest tail will be evicted.
        files.sort_by_key(|file| std::cmp::Reverse(file.1));
        let (to_keep, to_evict) = if files.len() > keep {
            files.split_at(keep)
        } else {
            (&files[..], &[][..])
        };

        let mut summary = RotateKindSummary {
            kept: to_keep.len(),
            ..Default::default()
        };
        for (path, _mtime, size) in to_evict {
            if delete_opted_in {
                // Legacy hard-delete behavior — only behind the explicit
                // `AM_BACKUP_ROTATION_DELETE` opt-in.
                match fs::remove_file(path) {
                    Ok(()) => {
                        debug!(kind = kind.label(), path = %path.display(), size, "deleted rotated backup (explicit opt-in)");
                        summary.deleted += 1;
                        summary.bytes_deleted = summary.bytes_deleted.saturating_add(*size);
                    }
                    Err(err) => {
                        warn!(
                            kind = kind.label(),
                            path = %path.display(),
                            %err,
                            "failed to delete rotated backup; keeping in place"
                        );
                        summary.kept += 1;
                    }
                }
                continue;
            }

            // Default: quarantine instead of delete (RULE 1). Move the file
            // into `doctor/reclaimable/rotation-<ts>/` so an operator (or a
            // later explicit reclaim) decides when disk is actually freed.
            if !quarantine_ready {
                if let Err(err) = fs::create_dir_all(&quarantine_dir) {
                    warn!(
                        dir = %quarantine_dir.display(),
                        %err,
                        "failed to create rotation quarantine dir; leaving evicted backups in place"
                    );
                    summary.kept += 1;
                    continue;
                }
                quarantine_ready = true;
            }
            let file_name = path.file_name().map_or_else(
                || std::ffi::OsString::from("unnamed-backup"),
                std::ffi::OsStr::to_os_string,
            );
            let dest = quarantine_dir.join(file_name);
            match fs::rename(path, &dest) {
                Ok(()) => {
                    debug!(kind = kind.label(), path = %path.display(), dest = %dest.display(), size, "staged rotated backup into quarantine");
                    summary.staged += 1;
                    summary.bytes_staged = summary.bytes_staged.saturating_add(*size);
                }
                Err(err) => {
                    // A cross-device rename can't succeed; a copy+remove
                    // fallback would still be a delete, so it stays behind
                    // the same explicit opt-in (which hard-deletes above
                    // anyway). Without the opt-in, leave the file in place
                    // and say so.
                    warn!(
                        kind = kind.label(),
                        path = %path.display(),
                        dest = %dest.display(),
                        %err,
                        "failed to stage rotated backup into quarantine; keeping in place"
                    );
                    summary.kept += 1;
                }
            }
        }

        report.kept = report.kept.saturating_add(summary.kept);
        report.staged = report.staged.saturating_add(summary.staged);
        report.deleted = report.deleted.saturating_add(summary.deleted);
        report.bytes_staged = report.bytes_staged.saturating_add(summary.bytes_staged);
        report.bytes_deleted = report.bytes_deleted.saturating_add(summary.bytes_deleted);
        report.per_kind.insert(kind.label(), summary);
    }

    if report.evicted() > 0 {
        info!(
            staged = report.staged,
            deleted = report.deleted,
            kept = report.kept,
            bytes_staged = report.bytes_staged,
            bytes_deleted = report.bytes_deleted,
            "rotated storage backups"
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::Duration;
    use tempfile::TempDir;

    fn touch(path: &Path, size: usize) {
        let mut f = fs::File::create(path).unwrap();
        if size > 0 {
            f.write_all(&vec![0u8; size]).unwrap();
        }
    }

    /// Rotate with the delete knob pinned off (the default). The env
    /// override map is process-global, so tests asserting quarantine
    /// behavior pin it explicitly rather than racing the opt-in test.
    fn rotate_with_delete_off(root: &Path, keep: usize) -> RotateReport {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || rotate_storage_backups(root, keep).expect("rotate"),
        )
    }

    #[test]
    fn classify_backup_file_matches_corrupt_variants() {
        assert_eq!(
            classify_backup_file("storage.sqlite3.corrupt-20260419_123456_789"),
            Some(BackupKind::Corrupt)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3-wal.corrupt-20260419_123456_789"),
            Some(BackupKind::Corrupt)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.corrupt-20260419_123456_789-shm"),
            Some(BackupKind::Corrupt)
        );
    }

    #[test]
    fn classify_backup_file_matches_reconstruct_variants() {
        assert_eq!(
            classify_backup_file("storage.sqlite3.reconstruct-failed-20260419_211115_221"),
            Some(BackupKind::Reconstruct)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.reconstructing-20260419_222625_181"),
            Some(BackupKind::Reconstruct)
        );
    }

    #[test]
    fn classify_backup_file_matches_archive_reconcile_variants() {
        assert_eq!(
            classify_backup_file("storage.sqlite3.archive-reconcile-20260419_211310_125"),
            Some(BackupKind::ArchiveReconcile)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.archive-reconcile-failed-20260330_022649_667"),
            Some(BackupKind::ArchiveReconcile)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.archive-reconcile-restore-20260418_063252_498"),
            Some(BackupKind::ArchiveReconcile)
        );
    }

    #[test]
    fn classify_backup_file_refuses_live_db_and_codex_sidecar() {
        assert_eq!(classify_backup_file("storage.sqlite3"), None);
        assert_eq!(classify_backup_file("storage.sqlite3-wal"), None);
        assert_eq!(classify_backup_file("storage.sqlite3-shm"), None);
        assert_eq!(classify_backup_file("mailbox.sqlite3"), None);
        assert_eq!(classify_backup_file("storage.codex.sqlite3"), None);
        assert_eq!(classify_backup_file("storage.codex.sqlite3-wal"), None);
    }

    #[test]
    fn classify_backup_file_refuses_unrelated_files() {
        // Anything that isn't a storage.sqlite3-family backup is ignored.
        assert_eq!(classify_backup_file("random.txt"), None);
        assert_eq!(classify_backup_file("projects"), None);
        assert_eq!(classify_backup_file(".env"), None);
        assert_eq!(classify_backup_file("cline.mcp.json"), None);
    }

    #[test]
    fn classify_backup_file_matches_legacy_bak_variants_but_not_lookalikes() {
        // Actual bak backups created by prior versions / ad-hoc tooling.
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak"),
            Some(BackupKind::ManualBackup)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak.20260326_153504"),
            Some(BackupKind::ManualBackup)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak-something"),
            Some(BackupKind::ManualBackup)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.manual-backup-20260402_232941"),
            Some(BackupKind::ManualBackup)
        );
        // False-positive guard: `backup-*` must NOT classify as a bak.
        // Previously an overly-broad `starts_with("bak")` would have matched
        // `backup-plan.txt` and caused rotation to delete it.
        assert_eq!(
            classify_backup_file("storage.sqlite3.backup-plan-2026"),
            None
        );
        assert_eq!(classify_backup_file("storage.sqlite3.backdoor-key"), None);
        assert_eq!(classify_backup_file("storage.sqlite3.bakers-list"), None);
    }

    #[test]
    fn classify_backup_file_matches_pre_migration_variants() {
        assert_eq!(
            classify_backup_file("storage.sqlite3.pre-migrate.20260324_040902.tmp.1122028"),
            Some(BackupKind::PreMigration)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.pre-python-import-20260321T155158Z"),
            Some(BackupKind::PreMigration)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.pre-acfs-import-20260312T174401Z.bak"),
            Some(BackupKind::PreMigration)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.pre-reindex-20260312T174618Z.bak"),
            Some(BackupKind::PreMigration)
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.salvage-20260329_021721_188.sqlite3"),
            Some(BackupKind::Salvage)
        );
    }

    #[test]
    fn rotate_storage_backups_keeps_newest_n_per_kind() {
        let tmp = TempDir::new().unwrap();
        // Make 5 corrupt backups with staggered mtimes.
        for i in 0..5 {
            let path = tmp
                .path()
                .join(format!("storage.sqlite3.corrupt-20260419_12000{i}_000"));
            touch(&path, 100);
            // Bump mtime ordering so "i" is the oldest, "4" is the newest.
            sleep(Duration::from_millis(5));
        }
        // And 2 reconstruct-failed — both should survive with keep=3.
        for i in 0..2 {
            let path = tmp.path().join(format!(
                "storage.sqlite3.reconstruct-failed-20260419_13000{i}_000"
            ));
            touch(&path, 50);
            sleep(Duration::from_millis(5));
        }
        // An unrelated file that must be left alone.
        touch(&tmp.path().join("do-not-touch.txt"), 7);

        let report = rotate_with_delete_off(tmp.path(), 3);

        assert_eq!(report.staged, 2, "expected 2 oldest corrupts staged");
        assert_eq!(report.deleted, 0, "nothing hard-deleted without opt-in");
        assert_eq!(report.kept, 3 + 2, "3 corrupts + 2 reconstructs kept");
        assert!(report.bytes_staged > 0);
        assert_eq!(report.bytes_deleted, 0);

        // Unrelated file intact.
        assert!(tmp.path().join("do-not-touch.txt").exists());

        // 3 corrupt files remain; which ones? The 3 newest (2, 3, 4).
        let mut remaining_corrupt: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| classify_backup_file(n) == Some(BackupKind::Corrupt))
            .collect();
        remaining_corrupt.sort();
        assert_eq!(remaining_corrupt.len(), 3);
    }

    #[test]
    fn rotate_storage_backups_does_not_remove_live_state() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("storage.sqlite3"), 1024);
        touch(&tmp.path().join("storage.sqlite3-wal"), 256);
        touch(&tmp.path().join("storage.sqlite3-shm"), 64);
        touch(&tmp.path().join("storage.codex.sqlite3"), 4096);

        let report = rotate_storage_backups(tmp.path(), 3).expect("rotate");
        assert_eq!(report.evicted(), 0);
        assert!(tmp.path().join("storage.sqlite3").exists());
        assert!(tmp.path().join("storage.sqlite3-wal").exists());
        assert!(tmp.path().join("storage.sqlite3-shm").exists());
        assert!(tmp.path().join("storage.codex.sqlite3").exists());
    }

    #[test]
    fn rotate_storage_backups_on_missing_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report = rotate_storage_backups(&missing, 3).expect("rotate");
        assert_eq!(report.evicted(), 0);
        assert_eq!(report.kept, 0);
    }

    #[test]
    fn rotate_storage_backups_respects_min_keep_floor() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            touch(
                &tmp.path()
                    .join(format!("storage.sqlite3.corrupt-20260419_12000{i}_000")),
                10,
            );
            sleep(Duration::from_millis(5));
        }
        // keep=0 should be clamped to MIN_KEEP_PER_KIND (=1)
        let report = rotate_with_delete_off(tmp.path(), 0);
        assert_eq!(report.kept, 1);
        assert_eq!(report.staged, 4);
        assert_eq!(report.deleted, 0);
    }

    /// Collect the file names inside the (single) `rotation-*` quarantine
    /// directory, or an empty vec when no quarantine dir exists yet.
    fn quarantined_names(storage_root: &Path) -> Vec<String> {
        let reclaimable = storage_root.join("doctor").join("reclaimable");
        let Ok(entries) = fs::read_dir(&reclaimable) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                dir_name.starts_with("rotation-"),
                "unexpected entry in doctor/reclaimable: {dir_name}"
            );
            for file in fs::read_dir(entry.path()).unwrap().flatten() {
                names.push(file.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        names
    }

    #[test]
    fn rotate_storage_backups_stages_instead_of_deleting() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            touch(
                &tmp.path()
                    .join(format!("storage.sqlite3.corrupt-20260419_12000{i}_000")),
                100,
            );
            sleep(Duration::from_millis(5));
        }

        let report = rotate_with_delete_off(tmp.path(), 3);
        assert_eq!(report.staged, 2);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.bytes_staged, 200);
        assert_eq!(report.bytes_deleted, 0);

        // The two oldest were moved out of the storage root, not deleted:
        // originals gone at the top level, contents intact in quarantine.
        assert!(
            !tmp.path()
                .join("storage.sqlite3.corrupt-20260419_120000_000")
                .exists()
        );
        assert!(
            !tmp.path()
                .join("storage.sqlite3.corrupt-20260419_120001_000")
                .exists()
        );
        let staged = quarantined_names(tmp.path());
        assert_eq!(
            staged,
            vec![
                "storage.sqlite3.corrupt-20260419_120000_000".to_string(),
                "storage.sqlite3.corrupt-20260419_120001_000".to_string(),
            ]
        );
        for name in &staged {
            let bytes = fs::metadata(
                tmp.path()
                    .join("doctor")
                    .join("reclaimable")
                    .join(quarantine_dir_name(tmp.path()))
                    .join(name),
            )
            .unwrap()
            .len();
            assert_eq!(bytes, 100, "staged file body must survive the move");
        }
    }

    /// Name of the single `rotation-*` quarantine dir under
    /// `doctor/reclaimable/` (panics if there isn't exactly one).
    fn quarantine_dir_name(storage_root: &Path) -> String {
        let mut dirs: Vec<String> = fs::read_dir(storage_root.join("doctor").join("reclaimable"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            dirs.len(),
            1,
            "expected exactly one rotation quarantine dir"
        );
        dirs.pop().unwrap()
    }

    #[test]
    fn rotation_delete_requires_explicit_optin() {
        let tmp = TempDir::new().unwrap();
        for i in 0..3 {
            touch(
                &tmp.path()
                    .join(format!("storage.sqlite3.corrupt-20260419_12000{i}_000")),
                10,
            );
            sleep(Duration::from_millis(5));
        }

        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "1")],
            || rotate_storage_backups(tmp.path(), 1).expect("rotate"),
        );

        assert_eq!(report.deleted, 2, "opt-in restores hard-delete");
        assert_eq!(report.staged, 0);
        assert_eq!(report.bytes_deleted, 20);
        // Hard-delete removes the files outright — no quarantine dir appears.
        assert!(!tmp.path().join("doctor").exists());
        assert!(
            !tmp.path()
                .join("storage.sqlite3.corrupt-20260419_120000_000")
                .exists()
        );
    }

    #[test]
    fn rotation_skips_archive_reconcile_backups() {
        let tmp = TempDir::new().unwrap();
        // Well past keep=1, but archive-reconcile snapshots are owned by the
        // recovery-retention reclaim planner — rotation must not touch them.
        for i in 0..4 {
            touch(
                &tmp.path().join(format!(
                    "storage.sqlite3.archive-reconcile-20260419_12000{i}_000"
                )),
                25,
            );
            sleep(Duration::from_millis(5));
        }

        let report = rotate_storage_backups(tmp.path(), 1).expect("rotate");
        assert_eq!(report.evicted(), 0);
        assert!(!report.per_kind.contains_key("archive_reconcile"));
        for i in 0..4 {
            assert!(
                tmp.path()
                    .join(format!(
                        "storage.sqlite3.archive-reconcile-20260419_12000{i}_000"
                    ))
                    .exists(),
                "archive-reconcile snapshot {i} must be left in place"
            );
        }
        assert!(!tmp.path().join("doctor").exists());
    }

    #[test]
    fn backup_inventory_excludes_live_database_and_reports_rotatable_bytes() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("storage.sqlite3"), 1_000);
        touch(
            &tmp.path()
                .join("storage.sqlite3.archive-reconcile-20260419_120000_000"),
            400,
        );
        touch(&tmp.path().join("unrelated.txt"), 600);

        let inventory = inspect_storage_backups(tmp.path()).expect("inventory");
        assert_eq!(inventory.artifact_count, 1);
        assert_eq!(inventory.resident_bytes, 400);
        assert_eq!(inventory.artifacts.len(), 1);
        assert!(
            inventory.artifacts[0]
                .path
                .ends_with("storage.sqlite3.archive-reconcile-20260419_120000_000")
        );
    }
}
