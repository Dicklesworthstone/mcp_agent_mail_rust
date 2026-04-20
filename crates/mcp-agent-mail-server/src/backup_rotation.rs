//! Bounded retention for storage-root backup detritus.
//!
//! Background: every SQLite corruption, reconstruction, or archive-reconcile
//! cycle creates a dated backup file (e.g., `storage.sqlite3.corrupt-20260419_...`).
//! Without rotation these accumulate forever. The 2026-04-19 incident had
//! 25 `.corrupt-*`, 17 `.reconstruct-failed-*`, and 40+ `.archive-reconcile-*`
//! files totaling ~1.3 GB in a single storage_root.
//!
//! This module keeps the N most recent of each *kind* and deletes the rest.
//! Kinds are classified by filename suffix pattern so we never touch the
//! live DB (`storage.sqlite3`), the Codex sidecar DB, or unrelated files.
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

/// Buckets that `classify_backup_file` maps filenames into. Each bucket has
/// an independent retention count — we don't want a corruption storm to
/// evict unrelated pre-migration backups, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackupKind {
    /// `storage.sqlite3.corrupt-*` (plus -wal/-shm siblings)
    Corrupt,
    /// `storage.sqlite3.reconstruct-failed-*` and `storage.sqlite3.reconstructing-*`
    Reconstruct,
    /// `storage.sqlite3.archive-reconcile-*` (incl. -failed, -restore)
    ArchiveReconcile,
    /// `storage.sqlite3.salvage-*`
    Salvage,
    /// `storage.sqlite3.manual-backup-*` plus bare `.bak*`
    ManualBackup,
    /// `.pre-migrate.*`, `.pre-python-import-*`, `.pre-acfs-import-*`, `.pre-reindex-*`
    PreMigration,
}

impl BackupKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Corrupt => "corrupt",
            Self::Reconstruct => "reconstruct",
            Self::ArchiveReconcile => "archive_reconcile",
            Self::Salvage => "salvage",
            Self::ManualBackup => "manual_backup",
            Self::PreMigration => "pre_migration",
        }
    }
}

/// Report returned by `rotate_storage_backups`. One entry per non-empty kind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateReport {
    pub kept: usize,
    pub removed: usize,
    pub bytes_reclaimed: u64,
    pub per_kind: BTreeMap<&'static str, RotateKindSummary>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateKindSummary {
    pub kept: usize,
    pub removed: usize,
    pub bytes_reclaimed: u64,
}

/// Classify a `storage_root`-relative filename into a `BackupKind`.
///
/// Returns `None` for the live DB, Codex DB, and any file that doesn't
/// match our known backup patterns. This is the *single* place that
/// decides whether rotation can touch a file.
#[must_use]
pub fn classify_backup_file(file_name: &str) -> Option<BackupKind> {
    // Explicitly never-delete live state.
    if matches!(
        file_name,
        "storage.sqlite3" | "storage.sqlite3-wal" | "storage.sqlite3-shm" | "mailbox.sqlite3"
    ) {
        return None;
    }
    // Leave the Codex sidecar DB alone.
    if file_name.starts_with("storage.codex.sqlite3") {
        return None;
    }

    // Only files named like the storage DB (or its WAL/SHM siblings) are
    // eligible. This is the primary guard against touching unrelated files.
    let stem_prefixes = [
        "storage.sqlite3.",
        "storage.sqlite3-wal.",
        "storage.sqlite3-shm.",
    ];
    let after_stem = stem_prefixes
        .iter()
        .find_map(|prefix| file_name.strip_prefix(prefix))?;

    // `archive-reconcile-` covers the bare, `-failed-*`, and `-restore-*`
    // variants in one check (they all share the prefix).
    if after_stem.starts_with("archive-reconcile-") {
        return Some(BackupKind::ArchiveReconcile);
    }
    if after_stem.starts_with("reconstruct-") || after_stem.starts_with("reconstructing-") {
        return Some(BackupKind::Reconstruct);
    }
    if after_stem.starts_with("corrupt-") {
        return Some(BackupKind::Corrupt);
    }
    if after_stem.starts_with("salvage-") {
        return Some(BackupKind::Salvage);
    }
    // Manual-backup naming is either `manual-backup-<ts>` or the bare `bak`
    // / `bak.<ts>` legacy variant. Using a bare `starts_with("bak")` would
    // false-positive future filenames like `backup-plan.txt` that happen to
    // share the prefix — match exact variants only.
    if after_stem.starts_with("manual-backup-")
        || after_stem == "bak"
        || after_stem.starts_with("bak.")
        || after_stem.starts_with("bak-")
    {
        return Some(BackupKind::ManualBackup);
    }
    if after_stem.starts_with("pre-migrate")
        || after_stem.starts_with("pre-python-import")
        || after_stem.starts_with("pre-acfs-import")
        || after_stem.starts_with("pre-reindex")
    {
        return Some(BackupKind::PreMigration);
    }

    None
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

/// Rotate backup files in `storage_root`, keeping `keep_per_kind` newest of
/// each kind and deleting the rest. Non-backup files (live DB, Codex DB,
/// projects/, search_index/, .git/, etc.) are never touched.
///
/// Returns a `RotateReport` with per-kind counts. Errors on individual
/// deletes are logged and counted as `kept` so partial failures don't mask
/// themselves.
pub fn rotate_storage_backups(
    storage_root: &Path,
    keep_per_kind: usize,
) -> std::io::Result<RotateReport> {
    let keep = keep_per_kind.max(MIN_KEEP_PER_KIND);

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
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        by_kind
            .entry(kind)
            .or_default()
            .push((entry.path(), mtime, meta.len()));
    }

    for (kind, mut files) in by_kind {
        // Sort descending by mtime — oldest tail will be deleted.
        files.sort_by(|a, b| b.1.cmp(&a.1));
        let (to_keep, to_delete) = if files.len() > keep {
            files.split_at(keep)
        } else {
            (&files[..], &[][..])
        };

        let mut summary = RotateKindSummary {
            kept: to_keep.len(),
            ..Default::default()
        };
        for (path, _mtime, size) in to_delete {
            match fs::remove_file(path) {
                Ok(()) => {
                    debug!(kind = kind.label(), path = %path.display(), size, "rotated backup");
                    summary.removed += 1;
                    summary.bytes_reclaimed = summary.bytes_reclaimed.saturating_add(*size);
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
        }

        report.kept = report.kept.saturating_add(summary.kept);
        report.removed = report.removed.saturating_add(summary.removed);
        report.bytes_reclaimed = report
            .bytes_reclaimed
            .saturating_add(summary.bytes_reclaimed);
        report.per_kind.insert(kind.label(), summary);
    }

    if report.removed > 0 {
        info!(
            removed = report.removed,
            kept = report.kept,
            bytes_reclaimed = report.bytes_reclaimed,
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

        let report = rotate_storage_backups(tmp.path(), 3).expect("rotate");

        assert_eq!(report.removed, 2, "expected 2 oldest corrupts removed");
        assert_eq!(report.kept, 3 + 2, "3 corrupts + 2 reconstructs kept");
        assert!(report.bytes_reclaimed > 0);

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
        assert_eq!(report.removed, 0);
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
        assert_eq!(report.removed, 0);
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
        let report = rotate_storage_backups(tmp.path(), 0).expect("rotate");
        assert_eq!(report.kept, 1);
        assert_eq!(report.removed, 4);
    }
}
