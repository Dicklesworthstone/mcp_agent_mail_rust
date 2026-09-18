//! Bounded retention for backups of the configured database.
//!
//! Background: every SQLite corruption, reconstruction, or archive-reconcile
//! cycle creates a dated backup file (e.g., `storage.sqlite3.corrupt-20260419_...`).
//! Without rotation these accumulate forever. The 2026-04-19 incident had
//! 25 `.corrupt-*`, 17 `.reconstruct-failed-*`, and 40+ `.archive-reconcile-*`
//! files totaling ~1.3 GB in a single storage_root.
//!
//! This module keeps the N most recent of each *kind* and quarantines the
//! rest by moving them into `doctor/reclaimable/rotation-<ts>[-<n>]/` inside the
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
const MAX_QUARANTINE_LEAF_ATTEMPTS: u32 = 128;

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
/// "Staged" means moved into a `doctor/reclaimable/rotation-<ts>[-<n>]/`
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

/// Create a quarantine directory owned exclusively by this rotation pass.
///
/// Rotation can run concurrently in two cold-starting processes. A shared
/// second-resolution directory would let both processes target the same file
/// name. Claiming the directory with `create_dir` isolates cooperative passes;
/// atomic no-replace leaf moves additionally protect against raced entries.
fn create_unique_quarantine_dir(parent: &Path, stem: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(parent)?;

    for suffix in 0_u32..=u32::from(u16::MAX) {
        let directory_name = if suffix == 0 {
            stem.to_string()
        } else {
            format!("{stem}-{suffix}")
        };
        let candidate = parent.join(directory_name);
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "no unique rotation quarantine directory available under {}",
            parent.display()
        ),
    ))
}

/// Move one backup without ever replacing an occupied quarantine leaf.
/// Unsupported atomic moves and cross-device moves fail without copying or
/// unlinking source evidence. Directory ownership is not a substitute for
/// this primitive: another process can populate a claimed directory later.
fn stage_backup_noreplace(source: &Path, directory: &Path) -> std::io::Result<PathBuf> {
    stage_backup_noreplace_with(
        source,
        directory,
        mcp_agent_mail_db::pool::rename_noreplace_preserving_source,
    )
}

fn stage_backup_noreplace_with<F>(
    source: &Path,
    directory: &Path,
    mut rename: F,
) -> std::io::Result<PathBuf>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let name = source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "backup has no file name")
    })?;
    // The inventory can be stale by the time rotation reaches this entry.
    // Never deliberately stage a symlink, directory, or other non-file.
    if !fs::symlink_metadata(source)?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rotation source is no longer a regular backup file",
        ));
    }
    for suffix in 0..MAX_QUARANTINE_LEAF_ATTEMPTS {
        let mut leaf = name.to_os_string();
        if suffix != 0 {
            leaf.push(format!(".{suffix}"));
        }
        let destination = directory.join(leaf);
        // No exists() preflight: even dangling symlinks occupy a name, and
        // only the atomic operation can exclude a concurrent publication.
        match rename(source, &destination) {
            Ok(()) => return Ok(destination),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "quarantine collision budget exhausted for {} under {}; source retained",
            source.display(),
            directory.display()
        ),
    ))
}

/// Rotate backup files beside `database_path`, staging evictions for reclaim.
///
/// Keeps `keep_per_kind` newest of each kind and stages the rest into
/// `<storage_root>/doctor/reclaimable/rotation-<UTC ts>[-<n>]/` for operator
/// reclaim. With the explicit `AM_BACKUP_ROTATION_DELETE` opt-in the evicted
/// files are hard-deleted instead (the legacy behavior).
/// An external database parent is inventoried in place; quarantine remains
/// under the archive root. Cross-device staging leaves the backup untouched
/// and reports it as kept, without a copy-and-delete fallback.
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
    database_path: &Path,
    keep_per_kind: usize,
) -> std::io::Result<RotateReport> {
    let keep = keep_per_kind.max(MIN_KEEP_PER_KIND);
    let delete_opted_in = rotation_delete_opted_in();
    let snapshot_primary = database_path;
    let Some(database_name) = database_path.file_name() else {
        return Ok(RotateReport::default());
    };
    let database_parent = database_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let snapshot_metadata = mcp_agent_mail_db::snapshot::snapshot_meta_path(snapshot_primary);
    let snapshot_authority_occupied = match fs::symlink_metadata(&snapshot_metadata) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    };
    let pinned_verified_snapshot = snapshot_authority_occupied
        .then(|| mcp_agent_mail_db::snapshot::verified_snapshot_source_path(snapshot_primary))
        .flatten();

    let mut report = RotateReport::default();
    let entries = match fs::read_dir(database_parent) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e),
    };

    // Group candidate files by kind so we can rotate each independently.
    let mut by_kind: BTreeMap<BackupKind, Vec<(PathBuf, SystemTime, u64)>> = BTreeMap::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            // `DirEntry::metadata()` follows symlinks. Check the directory
            // entry itself first so a backup-shaped symlink is never moved or
            // accounted as though its target were an owned backup file.
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Some(kind) = classify_backup_file(database_name, &entry.file_name()) else {
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
    let quarantine_parent = storage_root.join("doctor").join("reclaimable");
    let quarantine_stem = format!("rotation-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    let mut quarantine_dir = None;

    for (kind, mut files) in by_kind {
        if kind == BackupKind::ManualBackup
            && snapshot_authority_occupied
            && pinned_verified_snapshot.is_none()
        {
            let summary = RotateKindSummary {
                kept: files.len(),
                ..Default::default()
            };
            warn!(
                metadata = %snapshot_metadata.display(),
                kept = files.len(),
                "verified-snapshot metadata is occupied but no authoritative backup generation can be resolved; refusing manual-backup rotation"
            );
            report.kept = report.kept.saturating_add(summary.kept);
            report.per_kind.insert(kind.label(), summary);
            continue;
        }
        // Published backup generations outrank copy-preserved mtimes. Other
        // debris uses mtime; raw paths make ties deterministic.
        files.sort_by(|left, right| {
            let candidate = |path: &Path| {
                let name = path.file_name()?;
                ["", "-wal", "-shm"].into_iter().find_map(|sidecar| {
                    let mut stem = database_name.to_os_string();
                    stem.push(sidecar);
                    mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(&stem, name)
                })
            };
            let left_candidate = candidate(&left.0);
            let right_candidate = candidate(&right.0);
            let generation =
                |name: Option<mcp_agent_mail_core::disk::SqliteRecoveryCandidateName>, modified| {
                    name.and_then(
                        mcp_agent_mail_core::disk::SqliteRecoveryCandidateName::generation_micros,
                    )
                    .unwrap_or_else(|| {
                        chrono::DateTime::<chrono::Utc>::from(modified).timestamp_micros()
                    })
                };
            let order = generation(right_candidate, right.1)
                .cmp(&generation(left_candidate, left.1))
                .then_with(|| right_candidate.is_some().cmp(&left_candidate.is_some()));
            let collision_order = match (left_candidate, right_candidate) {
                (Some(left_name), Some(right_name)) => {
                    left_name.cmp_newest_first(left.1, right_name, right.1)
                }
                _ => std::cmp::Ordering::Equal,
            };
            order
                .then(collision_order)
                .then_with(|| left.0.cmp(&right.0))
        });
        let to_evict = files
            .iter()
            .enumerate()
            .filter(|(index, (path, _, _))| {
                if *index < keep {
                    return false;
                }
                let is_pinned = kind == BackupKind::ManualBackup
                    && pinned_verified_snapshot.as_ref().is_some_and(|pinned| {
                        fs::canonicalize(path)
                            .is_ok_and(|canonical| canonical.as_path() == pinned.as_path())
                    });
                !is_pinned
            })
            .map(|(_, file)| file)
            .collect::<Vec<_>>();

        let mut summary = RotateKindSummary {
            kept: files.len().saturating_sub(to_evict.len()),
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
            // into `doctor/reclaimable/rotation-<ts>[-<n>]/` so an operator (or a
            // later explicit reclaim) decides when disk is actually freed.
            if quarantine_dir.is_none() {
                match create_unique_quarantine_dir(&quarantine_parent, &quarantine_stem) {
                    Ok(directory) => quarantine_dir = Some(directory),
                    Err(err) => {
                        warn!(
                            parent = %quarantine_parent.display(),
                            %err,
                            "failed to claim a unique rotation quarantine dir; leaving evicted backups in place"
                        );
                        summary.kept += 1;
                        continue;
                    }
                }
            }
            let Some(directory) = quarantine_dir.as_ref() else {
                warn!(
                    path = %path.display(),
                    "rotation quarantine directory unexpectedly unavailable; keeping backup in place"
                );
                summary.kept += 1;
                continue;
            };
            match stage_backup_noreplace(path, directory) {
                Ok(dest) => {
                    debug!(kind = kind.label(), path = %path.display(), dest = %dest.display(), size, "staged rotated backup into quarantine");
                    summary.staged += 1;
                    summary.bytes_staged = summary.bytes_staged.saturating_add(*size);
                }
                Err(err) => {
                    // A cross-device or unsupported atomic move cannot
                    // safely fall back to copy+remove. Preserve the source.
                    warn!(
                        kind = kind.label(),
                        path = %path.display(),
                        directory = %directory.display(),
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

    fn classify_backup_file(name: &str) -> Option<BackupKind> {
        super::classify_backup_file(
            std::ffi::OsStr::new("storage.sqlite3"),
            std::ffi::OsStr::new(name),
        )
    }

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
            || rotate_storage_backups(root, &root.join("storage.sqlite3"), keep).expect("rotate"),
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
    fn classify_backup_file_matches_strict_bak_families_but_not_lookalikes() {
        // Published backup generations for the main database and its WAL/SHM
        // forensic families are recognized through the shared strict grammar.
        for name in [
            "storage.sqlite3.bak",
            "storage.sqlite3.bak.20260326_153504",
            "storage.sqlite3-wal.bak",
            "storage.sqlite3-wal.bak.20260326_153504",
            "storage.sqlite3-shm.bak",
            "storage.sqlite3-shm.bak.20260326_153504",
        ] {
            assert_eq!(
                classify_backup_file(name),
                Some(BackupKind::ManualBackup),
                "strict backup family should classify: {name}"
            );
        }
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak.meta.json"),
            None,
            "the canonical verified-snapshot authority is active control state, not disposable backup material"
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak.20260326_153504.meta.json"),
            None,
            "metadata-like companions must never be rotated independently of a generation"
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak.stage.tmp"),
            None,
            "malformed backup lookalikes are not retention-owned"
        );
        assert_eq!(
            classify_backup_file("storage.sqlite3.bak-something"),
            None,
            "unpublished bak-prefix lookalikes are not owned backup generations"
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
    fn rotation_pins_the_metadata_authorized_backup_generation() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        let database_dir = tmp.path().join("database");
        fs::create_dir(&database_dir).unwrap();
        let primary = database_dir.join("custom-mail.db");
        let conn = mcp_agent_mail_db::CanonicalDbConn::open_file(primary.to_str().unwrap())
            .expect("open mailbox fixture");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("initialize mailbox schema");
        conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[])
            .expect("checkpoint mailbox fixture");
        drop(conn);
        let exact = mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary);
        fs::copy(&primary, &exact).expect("copy verified exact backup");
        mcp_agent_mail_db::snapshot::record_snapshot_metadata(&primary, 42)
            .expect("record verified authority");
        let pinned = database_dir.join("custom-mail.db.bak.20260101_000000");
        fs::rename(&exact, &pinned).expect("rotate verified bytes");
        sleep(Duration::from_millis(5));
        touch(&database_dir.join("custom-mail.db.bak.20260102_000000"), 13);
        sleep(Duration::from_millis(5));
        touch(&database_dir.join("custom-mail.db.bak.20260103_000000"), 17);

        assert_eq!(inspect_storage_backups(&primary).unwrap().artifact_count, 3);
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || rotate_storage_backups(&archive, &primary, 1).unwrap(),
        );

        assert!(
            pinned.is_file(),
            "the verified hash source must remain live"
        );
        assert!(
            mcp_agent_mail_db::snapshot::snapshot_meta_path(&primary).is_file(),
            "canonical snapshot authority must never enter rotation"
        );
        assert_eq!(report.staged, 1);
        assert_eq!(report.kept, 2, "newest plus verified generation are kept");
        assert_eq!(inspect_storage_backups(&primary).unwrap().artifact_count, 2);
        assert_eq!(
            quarantined_names(&archive),
            ["custom-mail.db.bak.20260102_000000"]
        );
        assert_eq!(
            mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).as_deref(),
            Some(pinned.as_path())
        );
    }

    #[test]
    fn occupied_unresolvable_snapshot_metadata_parks_manual_backup_rotation() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        touch(&primary, 32);
        let metadata = mcp_agent_mail_db::snapshot::snapshot_meta_path(&primary);
        fs::write(&metadata, b"untrusted snapshot authority").unwrap();
        let backups = [
            tmp.path().join("storage.sqlite3.bak.20260101_000000"),
            tmp.path().join("storage.sqlite3.bak.20260102_000000"),
            tmp.path().join("storage.sqlite3.bak.20260103_000000"),
        ];
        for path in &backups {
            touch(path, 11);
            sleep(Duration::from_millis(5));
        }

        let report = rotate_with_delete_off(tmp.path(), 1);

        assert_eq!(report.staged, 0);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.kept, backups.len());
        assert_eq!(
            fs::read(&metadata).unwrap(),
            b"untrusted snapshot authority"
        );
        for path in backups {
            assert!(path.is_file());
        }
    }

    #[test]
    fn rotation_prefers_logical_generation_over_copied_mtime() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("custom.db");
        let newest = tmp.path().join("custom.db.bak.20260103_000000");
        let older = tmp.path().join("custom.db.bak.20260102_000000");
        let collision = tmp.path().join("custom.db.bak.20260103_000000-01");
        for (path, seconds) in [(&newest, 1), (&older, 3), (&collision, 2)] {
            touch(path, 17);
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        }
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || rotate_storage_backups(tmp.path(), &primary, 1).unwrap(),
        );
        assert_eq!(report.staged, 2);
        assert!(
            collision.is_file(),
            "latest logical generation and collision must survive"
        );
        assert!(!older.exists());
        assert!(!newest.exists());
    }

    #[test]
    fn rotation_breaks_equal_generation_ties_by_raw_path() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("custom.db");
        let main = tmp.path().join("custom.db.bak.20260103_000000");
        let wal = tmp.path().join("custom.db-wal.bak.20260103_000000");
        touch(&main, 11);
        touch(&wal, 13);
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || rotate_storage_backups(tmp.path(), &primary, 1).unwrap(),
        );
        assert_eq!(report.staged, 1);
        assert!(
            wal.is_file(),
            "equal generations use ascending raw path order"
        );
        assert!(!main.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rotation_matches_non_unicode_database_basename_without_aliasing() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = TempDir::new().unwrap();
        let name = std::ffi::OsString::from_vec(b"mail-\xff.db".to_vec());
        let primary = tmp.path().join(&name);
        let mut old_name = name.clone();
        old_name.push(".bak.20260101_000000");
        let mut new_name = name;
        new_name.push(".bak.20260102_000000");
        touch(&tmp.path().join(&old_name), 11);
        touch(&tmp.path().join(&new_name), 13);
        let alias = tmp.path().join("mail-�.db.bak.20260101_000000");
        touch(&alias, 19);
        assert_eq!(
            inspect_storage_backups(&primary).unwrap().resident_bytes,
            24
        );
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || rotate_storage_backups(tmp.path(), &primary, 1).unwrap(),
        );
        assert_eq!(report.staged, 1);
        assert!(tmp.path().join(new_name).is_file());
        assert_eq!(fs::read(alias).unwrap(), vec![0; 19]);
    }

    #[test]
    fn rotate_storage_backups_does_not_remove_live_state() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("storage.sqlite3"), 1024);
        touch(&tmp.path().join("storage.sqlite3-wal"), 256);
        touch(&tmp.path().join("storage.sqlite3-shm"), 64);
        touch(&tmp.path().join("storage.codex.sqlite3"), 4096);

        let report = rotate_storage_backups(tmp.path(), &tmp.path().join("storage.sqlite3"), 3)
            .expect("rotate");
        assert_eq!(report.evicted(), 0);
        assert!(tmp.path().join("storage.sqlite3").exists());
        assert!(tmp.path().join("storage.sqlite3-wal").exists());
        assert!(tmp.path().join("storage.sqlite3-shm").exists());
        assert!(tmp.path().join("storage.codex.sqlite3").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rotate_storage_backups_ignores_backup_shaped_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("outside-evidence");
        let real_backup = tmp.path().join("storage.sqlite3.corrupt-real");
        let linked_backup = tmp.path().join("storage.sqlite3.corrupt-linked");
        touch(&target, 23);
        touch(&real_backup, 11);
        symlink(&target, &linked_backup).unwrap();

        let report = rotate_with_delete_off(tmp.path(), 1);
        assert_eq!(report.staged, 0);
        assert_eq!(report.kept, 1);
        assert!(real_backup.is_file());
        assert!(
            linked_backup
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), vec![0_u8; 23]);
    }

    #[test]
    fn rotate_storage_backups_on_missing_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report =
            rotate_storage_backups(&missing, &missing.join("storage.sqlite3"), 3).expect("rotate");
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

    #[test]
    fn unique_quarantine_directory_never_reuses_an_existing_rotation() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("doctor").join("reclaimable");

        let first = create_unique_quarantine_dir(&parent, "rotation-fixed").unwrap();
        touch(&first.join("storage.sqlite3.corrupt-same-name"), 17);
        let second = create_unique_quarantine_dir(&parent, "rotation-fixed").unwrap();

        assert_ne!(first, second);
        assert_eq!(first.file_name().unwrap(), "rotation-fixed");
        assert_eq!(second.file_name().unwrap(), "rotation-fixed-1");
        assert_eq!(
            fs::read(first.join("storage.sqlite3.corrupt-same-name")).unwrap(),
            vec![0_u8; 17],
            "claiming a later rotation directory must not replace prior evidence"
        );
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
            || {
                rotate_storage_backups(tmp.path(), &tmp.path().join("storage.sqlite3"), 1)
                    .expect("rotate")
            },
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

        let report = rotate_storage_backups(tmp.path(), &tmp.path().join("storage.sqlite3"), 1)
            .expect("rotate");
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

        let inventory =
            inspect_storage_backups(&tmp.path().join("storage.sqlite3")).expect("inventory");
        assert_eq!(inventory.artifact_count, 1);
        assert_eq!(inventory.resident_bytes, 400);
        assert_eq!(inventory.artifacts.len(), 1);
        assert!(
            inventory.artifacts[0]
                .path
                .ends_with("storage.sqlite3.archive-reconcile-20260419_120000_000")
        );
    }

    #[test]
    fn staging_preserves_a_destination_created_at_the_move_boundary() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = root.path().join("quarantine");
        fs::create_dir(&directory).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        let mut attempts = 0;
        let staged = stage_backup_noreplace_with(&source, &directory, |from, to| {
            attempts += 1;
            if attempts == 1 {
                fs::write(to, b"concurrent evidence").unwrap();
            }
            mcp_agent_mail_db::pool::rename_noreplace_preserving_source(from, to)
        })
        .expect("retry the actual OS collision");
        assert_eq!(attempts, 2);
        assert_eq!(staged, directory.join("backup.1"));
        assert_eq!(fs::read(directory.join("backup")).unwrap(), b"concurrent evidence");
        assert_eq!(fs::read(staged).unwrap(), b"source evidence");
        assert!(!source.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_preserves_dangling_destination_symlink() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = root.path().join("quarantine");
        fs::create_dir(&directory).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        let occupied = directory.join("backup");
        std::os::unix::fs::symlink("absent-target", &occupied).unwrap();
        let staged = stage_backup_noreplace(&source, &directory).unwrap();
        assert_eq!(staged, directory.join("backup.1"));
        assert!(fs::symlink_metadata(&occupied).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(occupied).unwrap(), Path::new("absent-target"));
        assert_eq!(fs::read(staged).unwrap(), b"source evidence");
    }

    #[test]
    fn staging_collision_budget_exhaustion_preserves_all_evidence() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = root.path().join("quarantine");
        fs::create_dir(&directory).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        for suffix in 0..MAX_QUARANTINE_LEAF_ATTEMPTS {
            let leaf = if suffix == 0 { "backup".to_string() } else { format!("backup.{suffix}") };
            fs::write(directory.join(leaf), b"retained evidence").unwrap();
        }
        let error = stage_backup_noreplace(&source, &directory).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("collision budget exhausted"));
        assert_eq!(fs::read(source).unwrap(), b"source evidence");
        let entries: Vec<_> = fs::read_dir(&directory).unwrap().collect();
        assert_eq!(entries.len(), MAX_QUARANTINE_LEAF_ATTEMPTS as usize);
        for entry in entries {
            assert_eq!(fs::read(entry.unwrap().path()).unwrap(), b"retained evidence");
        }
    }

    #[test]
    fn staging_missing_destination_does_not_copy_or_remove_source() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        fs::write(&source, b"source evidence").unwrap();
        assert!(stage_backup_noreplace(&source, &root.path().join("absent")).is_err());
        assert_eq!(fs::read(source).unwrap(), b"source evidence");
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_source_replaced_by_a_symlink() {
        let root = TempDir::new().unwrap();
        let directory = root.path().join("quarantine");
        fs::create_dir(&directory).unwrap();
        let target = root.path().join("target");
        let source = root.path().join("backup");
        fs::write(&target, b"unrelated evidence").unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();
        assert!(stage_backup_noreplace(&source, &directory).is_err());
        assert!(fs::symlink_metadata(source).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"unrelated evidence");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn staging_collision_suffix_preserves_raw_filename_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let root = TempDir::new().unwrap();
        let directory = root.path().join("quarantine");
        fs::create_dir(&directory).unwrap();
        let name = std::ffi::OsString::from_vec(b"backup-\xff".to_vec());
        let source = root.path().join(&name);
        fs::write(&source, b"source").unwrap();
        fs::write(directory.join(&name), b"existing").unwrap();
        let staged = stage_backup_noreplace(&source, &directory).unwrap();
        let mut suffixed = name.clone();
        suffixed.push(".1");
        assert_eq!(staged, directory.join(suffixed));
        assert_eq!(fs::read(directory.join(name)).unwrap(), b"existing");
        assert_eq!(fs::read(staged).unwrap(), b"source");
    }
}
