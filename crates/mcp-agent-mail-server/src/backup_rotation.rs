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

#[cfg(unix)]
use mcp_agent_mail_db::recovery_retention::{
    CompletedReclaimMove, ReclaimDirectory as RotationQuarantine,
};

/// How many of each kind to keep when rotating. Override via
/// `AM_BACKUP_KEEP_COUNT`. Floor of 1 (keep the most recent no matter what).
const DEFAULT_KEEP_PER_KIND: usize = 3;
const MIN_KEEP_PER_KIND: usize = 1;
#[cfg(any(not(unix), test))]
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
/// Staged bytes are *not* reclaimed disk. On Unix, staging also requires the
/// source-file and parent-directory syncs and identity checks to complete.
/// A rename followed by failed sync or validation is separately unconfirmed:
/// it is neither a successful stage nor an entry left at its source.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateReport {
    /// Entries not moved or deleted by this pass, including refused operations.
    pub kept: usize,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
    pub unconfirmed_moves: usize,
    pub bytes_unconfirmed_moves: u64,
    pub failures: Vec<RotationFailure>,
    pub per_kind: BTreeMap<&'static str, RotateKindSummary>,
}

impl RotateReport {
    /// Confirmed completed evictions. Unconfirmed moves are reported separately
    /// and must never be silently promoted into successful staging counts.
    #[must_use]
    pub const fn evicted(&self) -> usize {
        self.staged.saturating_add(self.deleted)
    }
}

/// An incomplete operation, including whether the atomic rename took place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationFailure {
    pub source: PathBuf,
    pub rename_completed: bool,
    /// Requested spelling, not a guarantee that a concurrently renamed parent
    /// can still be reached through this path. Never use it for blind rollback.
    pub requested_destination: Option<PathBuf>,
    pub error: String,
}

impl RotationFailure {
    fn from_error(source: &Path, error: &std::io::Error) -> Self {
        #[cfg(unix)]
        let requested_destination = CompletedReclaimMove::from_io_error(error)
            .map(|completed| completed.requested_destination().to_path_buf());
        #[cfg(not(unix))]
        let requested_destination: Option<PathBuf> = None;
        Self {
            source: source.to_path_buf(),
            rename_completed: requested_destination.is_some(),
            requested_destination,
            error: error.to_string(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateKindSummary {
    pub kept: usize,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
    pub unconfirmed_moves: usize,
    pub bytes_unconfirmed_moves: u64,
}

impl RotateKindSummary {
    fn record_failure(&mut self, failure: &RotationFailure, bytes: u64) {
        if failure.rename_completed {
            self.unconfirmed_moves = self.unconfirmed_moves.saturating_add(1);
            self.bytes_unconfirmed_moves = self.bytes_unconfirmed_moves.saturating_add(bytes);
        } else {
            self.kept = self.kept.saturating_add(1);
        }
    }
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

// Preserve the existing non-Unix move contract. The stronger descriptor and
// directory-durability implementation is Unix-specific; no claim of equivalent
// Windows namespace protection is made by the shared report shape.
#[cfg(not(unix))]
#[derive(Debug)]
struct RotationQuarantine {
    path: PathBuf,
}

#[cfg(not(unix))]
impl RotationQuarantine {
    fn claim(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Claim a unique quarantine and keep its directory authority for the whole
/// rotation. Unix creation walks ancestors without following user-controlled
/// symlinks and persists every newly created directory entry privately.
fn create_unique_quarantine_dir(parent: &Path, stem: &str) -> std::io::Result<RotationQuarantine> {
    for suffix in 0_u32..=u32::from(u16::MAX) {
        let directory_name = if suffix == 0 {
            stem.to_string()
        } else {
            format!("{stem}-{suffix}")
        };
        let candidate = parent.join(directory_name);
        match RotationQuarantine::claim(&candidate) {
            Ok(directory) => return Ok(directory),
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

/// On Unix, stage the inventoried inode through retained directory handles.
/// Unsupported or cross-device moves never use a copy-and-delete fallback.
fn stage_backup_noreplace(
    source: &Path,
    directory: &RotationQuarantine,
    inventoried: &fs::Metadata,
) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        directory.stage_inventoried_file(source, inventoried)
    }
    #[cfg(not(unix))]
    {
        let _ = inventoried;
        let name = source.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "backup has no file name")
        })?;
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
            let destination = directory.path().join(leaf);
            match mcp_agent_mail_db::pool::rename_noreplace_preserving_source(source, &destination)
            {
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
                directory.path().display()
            ),
        ))
    }
}

/// Rotate backup files beside `database_path`, staging evictions for reclaim.
///
/// Keeps `keep_per_kind` newest of each kind and stages the rest into
/// `<storage_root>/doctor/reclaimable/rotation-<UTC ts>[-<n>]/` for operator
/// reclaim. With the explicit `AM_BACKUP_ROTATION_DELETE` opt-in the evicted
/// files are hard-deleted instead (the legacy behavior).
/// An external database parent is inventoried in place; quarantine remains
/// under the archive root. Cross-device staging reports a refusal without a
/// copy-and-delete fallback.
///
/// Non-backup files (live DB, Codex DB, projects/, search_index/, .git/,
/// etc.) are never touched. Rotation only applies to files classified as
/// backups. `storage.sqlite3.archive-reconcile-*` files are additionally
/// excluded even though they classify as backups — see the comment inside.
///
/// Unix staging verifies the original inventory, retains the claimed directory
/// across the batch, and distinguishes a refused operation from a completed
/// rename with unconfirmed durability. Both are recorded in `failures`.
/// The explicit hard-delete lane and non-Unix staging retain their prior
/// behavior; they do not inherit the Unix descriptor-bound guarantees.
pub fn rotate_storage_backups(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
) -> std::io::Result<RotateReport> {
    rotate_storage_backups_with(
        storage_root,
        database_path,
        keep_per_kind,
        stage_backup_noreplace,
    )
}

fn rotate_storage_backups_with<F>(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
    mut stage: F,
) -> std::io::Result<RotateReport>
where
    F: FnMut(&Path, &RotationQuarantine, &fs::Metadata) -> std::io::Result<PathBuf>,
{
    let keep = keep_per_kind.max(MIN_KEEP_PER_KIND);
    let delete_opted_in = rotation_delete_opted_in();
    if database_path.file_name().is_none() {
        return Ok(RotateReport::default());
    }
    // One working-directory snapshot prevents a process-wide chdir from
    // redirecting later inventory entries. Do not canonicalize away symlinks.
    let cwd = if storage_root.is_relative() || database_path.is_relative() {
        Some(std::env::current_dir()?)
    } else {
        None
    };
    let absolute = |path: &Path| {
        if path.is_relative() {
            cwd.as_ref()
                .map_or_else(|| path.to_path_buf(), |cwd| cwd.join(path))
        } else {
            path.to_path_buf()
        }
    };
    let storage_root = absolute(storage_root);
    let database_path = absolute(database_path);
    let snapshot_primary = database_path.as_path();
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

    // Retain the inventory's metadata, not only its size. Staging must not
    // silently adopt a different file subsequently placed at this pathname.
    let mut by_kind: BTreeMap<BackupKind, Vec<(PathBuf, SystemTime, fs::Metadata)>> =
        BTreeMap::new();
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
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
            .push((entry.path(), mtime, meta));
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
        for (path, _mtime, metadata) in to_evict {
            let size = metadata.len();
            if delete_opted_in {
                // Legacy hard-delete behavior — only behind the explicit
                // `AM_BACKUP_ROTATION_DELETE` opt-in.
                match fs::remove_file(path) {
                    Ok(()) => {
                        debug!(kind = kind.label(), path = %path.display(), size, "deleted rotated backup (explicit opt-in)");
                        summary.deleted += 1;
                        summary.bytes_deleted = summary.bytes_deleted.saturating_add(size);
                    }
                    Err(err) => {
                        warn!(
                            kind = kind.label(),
                            path = %path.display(),
                            %err,
                            "failed to delete rotated backup; keeping in place"
                        );
                        let failure = RotationFailure::from_error(path, &err);
                        summary.record_failure(&failure, size);
                        report.failures.push(failure);
                    }
                }
                continue;
            }

            // Default: quarantine instead of delete (RULE 1). Keep the same
            // claimed directory handle across every kind and every artifact.
            if quarantine_dir.is_none() {
                match create_unique_quarantine_dir(&quarantine_parent, &quarantine_stem) {
                    Ok(directory) => quarantine_dir = Some(directory),
                    Err(err) => {
                        warn!(
                            parent = %quarantine_parent.display(),
                            %err,
                            "failed to claim a rotation quarantine; no backup was moved"
                        );
                        let failure = RotationFailure::from_error(path, &err);
                        summary.record_failure(&failure, size);
                        report.failures.push(failure);
                        continue;
                    }
                }
            }
            let Some(directory) = quarantine_dir.as_ref() else {
                let error = std::io::Error::other("rotation quarantine directory unavailable");
                let failure = RotationFailure::from_error(path, &error);
                summary.record_failure(&failure, size);
                report.failures.push(failure);
                continue;
            };
            match stage(path, directory, metadata) {
                Ok(dest) => {
                    debug!(kind = kind.label(), path = %path.display(), dest = %dest.display(), size, "staged rotated backup into quarantine");
                    summary.staged += 1;
                    summary.bytes_staged = summary.bytes_staged.saturating_add(size);
                }
                Err(err) => {
                    let failure = RotationFailure::from_error(path, &err);
                    if failure.rename_completed {
                        warn!(
                            kind = kind.label(),
                            path = %path.display(),
                            requested_destination = ?failure.requested_destination,
                            %err,
                            "backup rename completed but durability or namespace is unconfirmed; evidence retained, no rollback or retry"
                        );
                    } else {
                        warn!(
                            kind = kind.label(),
                            path = %path.display(),
                            directory = %directory.path().display(),
                            %err,
                            "rotation refused to stage this backup; no move performed by this attempt"
                        );
                    }
                    summary.record_failure(&failure, size);
                    report.failures.push(failure);
                }
            }
        }

        report.kept = report.kept.saturating_add(summary.kept);
        report.staged = report.staged.saturating_add(summary.staged);
        report.deleted = report.deleted.saturating_add(summary.deleted);
        report.bytes_staged = report.bytes_staged.saturating_add(summary.bytes_staged);
        report.bytes_deleted = report.bytes_deleted.saturating_add(summary.bytes_deleted);
        report.unconfirmed_moves = report
            .unconfirmed_moves
            .saturating_add(summary.unconfirmed_moves);
        report.bytes_unconfirmed_moves = report
            .bytes_unconfirmed_moves
            .saturating_add(summary.bytes_unconfirmed_moves);
        report.per_kind.insert(kind.label(), summary);
    }

    if report.evicted() > 0 || report.unconfirmed_moves > 0 {
        info!(
            staged = report.staged,
            deleted = report.deleted,
            kept = report.kept,
            bytes_staged = report.bytes_staged,
            bytes_deleted = report.bytes_deleted,
            unconfirmed_moves = report.unconfirmed_moves,
            bytes_unconfirmed_moves = report.bytes_unconfirmed_moves,
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
        assert_eq!(report.unconfirmed_moves, 0);
        assert_eq!(report.failures, Vec::new());

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
        touch(&first.path().join("storage.sqlite3.corrupt-same-name"), 17);
        let second = create_unique_quarantine_dir(&parent, "rotation-fixed").unwrap();

        assert_ne!(first.path(), second.path());
        assert_eq!(first.path().file_name().unwrap(), "rotation-fixed");
        assert_eq!(second.path().file_name().unwrap(), "rotation-fixed-1");
        assert_eq!(
            fs::read(first.path().join("storage.sqlite3.corrupt-same-name")).unwrap(),
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
    fn staging_preserves_a_destination_created_after_quarantine_claim() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        // The shared DB namespace tests additionally inject collisions at the
        // rename callback itself. This uses the actual production stage API.
        fs::write(directory.path().join("backup"), b"concurrent evidence").unwrap();
        let staged = stage_backup_noreplace(&source, &directory, &inventoried)
            .expect("retry the actual OS collision");
        assert_eq!(staged, directory.path().join("backup.1"));
        assert_eq!(
            fs::read(directory.path().join("backup")).unwrap(),
            b"concurrent evidence"
        );
        assert_eq!(fs::read(staged).unwrap(), b"source evidence");
        assert!(!source.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_preserves_dangling_destination_symlink() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        let occupied = directory.path().join("backup");
        std::os::unix::fs::symlink("absent-target", &occupied).unwrap();
        let staged = stage_backup_noreplace(&source, &directory, &inventoried).unwrap();
        assert_eq!(staged, directory.path().join("backup.1"));
        assert!(
            fs::symlink_metadata(&occupied)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(occupied).unwrap(), Path::new("absent-target"));
        assert_eq!(fs::read(staged).unwrap(), b"source evidence");
    }

    #[test]
    fn staging_collision_budget_exhaustion_preserves_all_evidence() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        fs::write(&source, b"source evidence").unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        for suffix in 0..MAX_QUARANTINE_LEAF_ATTEMPTS {
            let leaf = if suffix == 0 {
                "backup".to_string()
            } else {
                format!("backup.{suffix}")
            };
            fs::write(directory.path().join(leaf), b"retained evidence").unwrap();
        }
        let error = stage_backup_noreplace(&source, &directory, &inventoried).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("collision budget exhausted"));
        assert_eq!(fs::read(source).unwrap(), b"source evidence");
        let entries: Vec<_> = fs::read_dir(directory.path()).unwrap().collect();
        assert_eq!(entries.len(), MAX_QUARANTINE_LEAF_ATTEMPTS as usize);
        for entry in entries {
            assert_eq!(
                fs::read(entry.unwrap().path()).unwrap(),
                b"retained evidence"
            );
        }
    }

    #[test]
    fn staging_missing_destination_does_not_copy_or_remove_source() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("backup");
        fs::write(&source, b"source evidence").unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        let retained = root.path().join("retained-quarantine");
        fs::rename(directory.path(), &retained).unwrap();
        assert!(stage_backup_noreplace(&source, &directory, &inventoried).is_err());
        assert_eq!(fs::read(source).unwrap(), b"source evidence");
        assert_eq!(fs::read_dir(retained).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_source_replaced_by_a_symlink() {
        let root = TempDir::new().unwrap();
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        let target = root.path().join("target");
        let source = root.path().join("backup");
        fs::write(&target, b"unrelated evidence").unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        assert!(stage_backup_noreplace(&source, &directory, &inventoried).is_err());
        assert!(
            fs::symlink_metadata(source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), b"unrelated evidence");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn staging_collision_suffix_preserves_raw_filename_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let root = TempDir::new().unwrap();
        let directory = RotationQuarantine::claim(&root.path().join("quarantine")).unwrap();
        let name = std::ffi::OsString::from_vec(b"backup-\xff".to_vec());
        let source = root.path().join(&name);
        fs::write(&source, b"source").unwrap();
        let inventoried = fs::symlink_metadata(&source).unwrap();
        fs::write(directory.path().join(&name), b"existing").unwrap();
        let staged = stage_backup_noreplace(&source, &directory, &inventoried).unwrap();
        let mut suffixed = name.clone();
        suffixed.push(".1");
        assert_eq!(staged, directory.path().join(suffixed));
        assert_eq!(fs::read(directory.path().join(name)).unwrap(), b"existing");
        assert_eq!(fs::read(staged).unwrap(), b"source");
    }

    #[cfg(unix)]
    fn ordered_backup_pair(root: &Path) -> (PathBuf, PathBuf) {
        let old = root.join("storage.sqlite3.corrupt-old");
        let new = root.join("storage.sqlite3.corrupt-new");
        for (path, seconds) in [(&old, 1), (&new, 2)] {
            fs::write(path, b"original").unwrap();
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
        (old, new)
    }

    #[cfg(unix)]
    #[test]
    fn rotation_rejects_symlinked_quarantine_ancestors_without_touching_outside_files() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (old, new) = ordered_backup_pair(root.path());
        std::os::unix::fs::symlink(outside.path(), root.path().join("doctor")).unwrap();
        let report = rotate_with_delete_off(root.path(), 1);
        assert_eq!(report.kept, 2);
        assert_eq!(report.staged, 0);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.unconfirmed_moves, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(!report.failures[0].rename_completed);
        assert_eq!(fs::read(old).unwrap(), b"original");
        assert_eq!(fs::read(new).unwrap(), b"original");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rotation_rejects_same_size_same_mtime_replacement_after_inventory() {
        let root = TempDir::new().unwrap();
        let (old, new) = ordered_backup_pair(root.path());
        let retained = root.path().join("retained-original");
        let mut attempts = 0;
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || {
                rotate_storage_backups_with(
                    root.path(),
                    &root.path().join("storage.sqlite3"),
                    1,
                    |source, directory, metadata| {
                        attempts += 1;
                        assert_eq!(source, old);
                        fs::rename(source, &retained).unwrap();
                        fs::write(source, b"replaced").unwrap();
                        fs::File::options()
                            .write(true)
                            .open(source)
                            .unwrap()
                            .set_times(
                                fs::FileTimes::new().set_modified(metadata.modified().unwrap()),
                            )
                            .unwrap();
                        stage_backup_noreplace(source, directory, metadata)
                    },
                )
                .unwrap()
            },
        );
        assert_eq!(attempts, 1);
        assert_eq!(report.kept, 2);
        assert_eq!(report.staged, 0);
        assert_eq!(report.unconfirmed_moves, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(!report.failures[0].rename_completed);
        assert!(report.failures[0].requested_destination.is_none());
        assert_eq!(fs::read(old).unwrap(), b"replaced");
        assert_eq!(fs::read(retained).unwrap(), b"original");
        assert_eq!(fs::read(new).unwrap(), b"original");
        assert_eq!(quarantined_names(root.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn rotation_refuses_a_backup_hardlinked_to_the_live_mailbox() {
        let root = TempDir::new().unwrap();
        let primary = root.path().join("storage.sqlite3");
        fs::write(&primary, b"live mailbox state").unwrap();
        let alias = root.path().join("storage.sqlite3.corrupt-old");
        fs::hard_link(&primary, &alias).unwrap();
        fs::File::options()
            .write(true)
            .open(&alias)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            )
            .unwrap();
        let newest = root.path().join("storage.sqlite3.corrupt-new");
        fs::write(&newest, b"newest backup").unwrap();
        let report = rotate_with_delete_off(root.path(), 1);
        assert_eq!(report.kept, 2);
        assert_eq!(report.staged, 0);
        assert_eq!(report.unconfirmed_moves, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].error.contains("single-link"));
        assert_eq!(fs::read(primary).unwrap(), b"live mailbox state");
        assert_eq!(fs::read(alias).unwrap(), b"live mailbox state");
        assert_eq!(fs::read(newest).unwrap(), b"newest backup");
    }

    #[cfg(unix)]
    #[test]
    fn rotation_retains_the_claimed_quarantine_across_dispatch() {
        let root = TempDir::new().unwrap();
        let (old, new) = ordered_backup_pair(root.path());
        let retained = root.path().join("retained-quarantine");
        let mut requested = None;
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || {
                rotate_storage_backups_with(
                    root.path(),
                    &root.path().join("storage.sqlite3"),
                    1,
                    |source, directory, metadata| {
                        let replacement = directory.path().to_path_buf();
                        fs::rename(&replacement, &retained).unwrap();
                        fs::create_dir(&replacement).unwrap();
                        fs::write(
                            replacement.join(source.file_name().unwrap()),
                            b"outside sentinel",
                        )
                        .unwrap();
                        requested = Some(replacement);
                        stage_backup_noreplace(source, directory, metadata)
                    },
                )
                .unwrap()
            },
        );
        assert_eq!(report.kept, 2);
        assert_eq!(report.staged, 0);
        assert_eq!(report.unconfirmed_moves, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(fs::read(&old).unwrap(), b"original");
        assert_eq!(fs::read(new).unwrap(), b"original");
        assert_eq!(
            fs::read(requested.unwrap().join(old.file_name().unwrap())).unwrap(),
            b"outside sentinel"
        );
        assert_eq!(fs::read_dir(retained).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rotation_creates_private_directories_and_preserves_existing_root_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new().unwrap();
        let (old, new) = ordered_backup_pair(root.path());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let report = rotate_with_delete_off(root.path(), 1);
        assert_eq!(report.staged, 1);
        assert_eq!(report.bytes_staged, 8);
        assert_eq!(report.failures, Vec::new());
        assert_eq!(report.unconfirmed_moves, 0);
        let doctor = root.path().join("doctor");
        let reclaimable = doctor.join("reclaimable");
        let quarantine = reclaimable.join(quarantine_dir_name(root.path()));
        for directory in [&doctor, &reclaimable, &quarantine] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!old.exists());
        assert_eq!(
            fs::read(quarantine.join(old.file_name().unwrap())).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(new).unwrap(), b"original");
    }

    #[test]
    fn incomplete_move_accounting_never_claims_kept_or_successful_staging() {
        let mut summary = RotateKindSummary::default();
        let refused = RotationFailure::from_error(
            Path::new("source"),
            &std::io::Error::other("refused before rename"),
        );
        assert!(!refused.rename_completed);
        summary.record_failure(&refused, 7);
        let completed = RotationFailure {
            source: PathBuf::from("source"),
            rename_completed: true,
            requested_destination: Some(PathBuf::from("quarantine/source")),
            error: "post-move validation failure".to_string(),
        };
        summary.record_failure(&completed, 11);
        assert_eq!(summary.kept, 1);
        assert_eq!(summary.staged, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.bytes_staged, 0);
        assert_eq!(summary.unconfirmed_moves, 1);
        assert_eq!(summary.bytes_unconfirmed_moves, 11);
        let report = RotateReport {
            unconfirmed_moves: 1,
            bytes_unconfirmed_moves: 11,
            ..RotateReport::default()
        };
        assert_eq!(report.evicted(), 0);
    }
}
