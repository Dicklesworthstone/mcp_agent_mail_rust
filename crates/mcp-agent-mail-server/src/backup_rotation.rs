//! Bounded retention for backups of the configured database.
//!
//! Background: every SQLite corruption, reconstruction, or archive-reconcile
//! cycle creates a dated backup file (e.g., `storage.sqlite3.corrupt-20260419_...`).
//! Without rotation these accumulate forever. The 2026-04-19 incident had
//! 25 `.corrupt-*`, 17 `.reconstruct-failed-*`, and 40+ `.archive-reconcile-*`
//! files totaling ~1.3 GB in a single storage_root.
//!
//! This module keeps the N most recent of each *kind* and quarantines the
//! rest by moving incident evidence into `doctor/reclaimable/rotation-<ts>[-<n>]/`.
//! On Unix, dated periodic `.bak` generations are instead deleted by default
//! only after a newer fully verified snapshot authorizes reclamation. Previously
//! staged periodic generations are reclaimed by the same safety boundary.
//! An explicit false `AM_BACKUP_ROTATION_DELETE` preserves move-only retention;
//! the automatic producer then stops at its publication budget. Kinds are classified by filename
//! suffix pattern so we never touch the live DB (`storage.sqlite3`), the
//! Codex sidecar DB, or unrelated files.
//!
//! Default `keep_per_kind = 3`; override via `AM_BACKUP_KEEP_COUNT`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tracing::{debug, info, warn};

#[cfg(unix)]
use mcp_agent_mail_db::recovery_retention::{
    CompletedReclaimMove, ReclaimDirectory as RotationQuarantine,
};

/// How many of each kind to keep when rotating. Override via
/// `AM_BACKUP_KEEP_COUNT`. Floor of 1 (keep the most recent no matter what).
const DEFAULT_KEEP_PER_KIND: usize = 3;
const MIN_KEEP_PER_KIND: usize = 1;
const PERIODIC_BYTES_FLOOR: u64 = 512 * 1024 * 1024;
#[cfg(unix)]
const MAX_RECLAIM_FILES_PER_PASS: usize = 128;
#[cfg(unix)]
const MAX_RECLAIM_DIRECTORIES_PER_PASS: usize = 128;
/// Wall-clock limit on the rotation pass a cold start waits for.
///
/// That pass runs before the listener binds, and every reclaimed entry costs a
/// scan of `/proc`. Past this budget the pass stops reclaiming and the
/// background loop takes the rest.
pub const COLD_START_ROTATION_BUDGET: Duration = Duration::from_secs(1);
const MAX_INVENTORY_ENTRIES: usize = 16_384;
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
/// quarantine directory; "deleted" means hard-removed. Superseded periodic
/// generations can be deleted by default under verified snapshot admission;
/// other kinds require explicit `AM_BACKUP_ROTATION_DELETE` opt-in.
/// Staged bytes are *not* reclaimed disk. On Unix, staging also requires the
/// source-file and parent-directory syncs and identity checks to complete.
/// A rename followed by failed sync or validation is separately unconfirmed:
/// it is neither a successful stage nor an entry left at its source.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateReport {
    /// Empty native rotation directories durably removed, separate from
    /// backup payload counts and bytes. Nonempty recovery evidence is retained.
    pub empty_directories_deleted: usize,
    /// Entries not moved or deleted by this pass, including refused operations.
    pub kept: usize,
    /// Observed logical lengths still retained at their inventoried paths.
    pub bytes_kept: u64,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
    /// Allocated bytes of successfully unlinked, single-link files. This is
    /// not a measurement of the filesystem's free-space delta.
    pub bytes_reclaimed: u64,
    pub unconfirmed_deletions: usize,
    pub bytes_unconfirmed_deletions: u64,
    pub unconfirmed_moves: usize,
    pub bytes_unconfirmed_moves: u64,
    /// At least one path could not be inventoried. Byte totals are then known
    /// lower bounds; inaccessible evidence is never treated as zero bytes.
    pub inventory_incomplete: bool,
    /// Namespace, inventory, or snapshot admission was unavailable. A quota
    /// refusal following this pass must not authorize a repair export.
    pub admission_refused: bool,
    /// The pass reached its wall-clock budget. What it did not reach stays on
    /// disk, counted as kept, for a later pass.
    pub deadline_reached: bool,
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

    /// Known retained logical bytes, including quarantined and incompletely
    /// staged files. Unconfirmed unlinks have their own separate counter.
    #[must_use]
    pub const fn bytes_retained(&self) -> u64 {
        self.bytes_kept
            .saturating_add(self.bytes_staged)
            .saturating_add(self.bytes_unconfirmed_moves)
    }

    fn add_kind(&mut self, kind: &'static str, summary: &RotateKindSummary) {
        if summary == &RotateKindSummary::default() {
            return;
        }
        self.kept = self.kept.saturating_add(summary.kept);
        self.bytes_kept = self.bytes_kept.saturating_add(summary.bytes_kept);
        self.staged = self.staged.saturating_add(summary.staged);
        self.deleted = self.deleted.saturating_add(summary.deleted);
        self.bytes_staged = self.bytes_staged.saturating_add(summary.bytes_staged);
        self.bytes_deleted = self.bytes_deleted.saturating_add(summary.bytes_deleted);
        self.bytes_reclaimed = self.bytes_reclaimed.saturating_add(summary.bytes_reclaimed);
        self.unconfirmed_moves = self
            .unconfirmed_moves
            .saturating_add(summary.unconfirmed_moves);
        self.bytes_unconfirmed_moves = self
            .bytes_unconfirmed_moves
            .saturating_add(summary.bytes_unconfirmed_moves);
        self.unconfirmed_deletions = self
            .unconfirmed_deletions
            .saturating_add(summary.unconfirmed_deletions);
        self.bytes_unconfirmed_deletions = self
            .bytes_unconfirmed_deletions
            .saturating_add(summary.bytes_unconfirmed_deletions);
        let total = self.per_kind.entry(kind).or_default();
        total.kept = total.kept.saturating_add(summary.kept);
        total.bytes_kept = total.bytes_kept.saturating_add(summary.bytes_kept);
        total.staged = total.staged.saturating_add(summary.staged);
        total.deleted = total.deleted.saturating_add(summary.deleted);
        total.bytes_staged = total.bytes_staged.saturating_add(summary.bytes_staged);
        total.bytes_deleted = total.bytes_deleted.saturating_add(summary.bytes_deleted);
        total.bytes_reclaimed = total
            .bytes_reclaimed
            .saturating_add(summary.bytes_reclaimed);
        total.unconfirmed_moves = total
            .unconfirmed_moves
            .saturating_add(summary.unconfirmed_moves);
        total.bytes_unconfirmed_moves = total
            .bytes_unconfirmed_moves
            .saturating_add(summary.bytes_unconfirmed_moves);
        total.unconfirmed_deletions = total
            .unconfirmed_deletions
            .saturating_add(summary.unconfirmed_deletions);
        total.bytes_unconfirmed_deletions = total
            .bytes_unconfirmed_deletions
            .saturating_add(summary.bytes_unconfirmed_deletions);
    }
}

/// An incomplete operation, including whether the atomic rename took place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationFailure {
    pub source: PathBuf,
    pub rename_completed: bool,
    pub deletion_completed: bool,
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
            #[cfg(unix)]
            deletion_completed:
                mcp_agent_mail_db::recovery_retention::CompletedReclaimDelete::from_io_error(error)
                    .is_some(),
            #[cfg(not(unix))]
            deletion_completed: false,
            requested_destination,
            error: error.to_string(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RotateKindSummary {
    pub kept: usize,
    pub bytes_kept: u64,
    pub staged: usize,
    pub deleted: usize,
    pub bytes_staged: u64,
    pub bytes_deleted: u64,
    pub bytes_reclaimed: u64,
    pub unconfirmed_deletions: usize,
    pub bytes_unconfirmed_deletions: u64,
    pub unconfirmed_moves: usize,
    pub bytes_unconfirmed_moves: u64,
}

impl RotateKindSummary {
    fn record_failure(&mut self, failure: &RotationFailure, bytes: u64) {
        if failure.deletion_completed {
            self.unconfirmed_deletions = self.unconfirmed_deletions.saturating_add(1);
            self.bytes_unconfirmed_deletions =
                self.bytes_unconfirmed_deletions.saturating_add(bytes);
        } else if failure.rename_completed {
            self.unconfirmed_moves = self.unconfirmed_moves.saturating_add(1);
            self.bytes_unconfirmed_moves = self.bytes_unconfirmed_moves.saturating_add(bytes);
        } else {
            self.kept = self.kept.saturating_add(1);
            self.bytes_kept = self.bytes_kept.saturating_add(bytes);
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
/// Read from `AM_BACKUP_ROTATION_DELETE`. This controls legacy incident
/// deletion. Periodic generations use a separate verified reclamation lane:
/// unset enables it, explicit false disables it.
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

/// Periodic snapshots reclaim superseded bytes by default.
///
/// An explicit false retains the operator's move-only policy; publication
/// admission then bounds further growth instead of silently ignoring that
/// policy.
#[must_use]
pub fn periodic_reclamation_enabled() -> bool {
    mcp_agent_mail_core::config::process_env_value("AM_BACKUP_ROTATION_DELETE")
        .is_none_or(|_| rotation_delete_opted_in())
}

#[derive(Debug)]
struct PeriodicBudgetExceeded(String);

impl std::fmt::Display for PeriodicBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PeriodicBudgetExceeded {}

/// Only a complete inventory that exceeds policy may use the durable repair
/// allowance. I/O failures and ambiguous paths never authorize extra exports.
pub(crate) fn periodic_publication_budget_exhausted(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<PeriodicBudgetExceeded>())
}

fn is_periodic_backup(primary: &Path, name: &std::ffi::OsStr) -> bool {
    primary
        .file_name()
        .and_then(|database_name| {
            mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(database_name, name)
        })
        .is_some_and(|candidate| {
            matches!(
                candidate.kind(),
                mcp_agent_mail_core::disk::SqliteRecoveryCandidateKind::ProactiveBak
                    | mcp_agent_mail_core::disk::SqliteRecoveryCandidateKind::TimestampedBak
            )
        })
}

/// Whether a file of one backup kind occupies that kind's resident window and
/// can be evicted from it. Under periodic reclamation the manual-backup kind
/// counts only its strict periodic generations. Operator exports and WAL/SHM
/// backup copies stay where they are: the deletion guard refuses them on every
/// pass, and staging them would charge their bytes to the publication budget.
fn occupies_rotation_window(primary: &Path, path: &Path, periodic_reclaim: bool) -> bool {
    !periodic_reclaim
        || path
            .file_name()
            .is_some_and(|name| is_periodic_backup(primary, name))
}

fn is_native_rotation_directory(name: &std::ffi::OsStr) -> bool {
    let Some(suffix) = name
        .to_str()
        .and_then(|name| name.strip_prefix("rotation-"))
    else {
        return false;
    };
    let Some(timestamp) = suffix.get(..16) else {
        return false;
    };
    if chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ").is_err() {
        return false;
    }
    let tail = &suffix[16..];
    tail.is_empty()
        || tail.strip_prefix('-').is_some_and(|collision| {
            !collision.is_empty()
                && !collision.starts_with('0')
                && collision.bytes().all(|byte| byte.is_ascii_digit())
                && collision.parse::<u16>().is_ok()
        })
}

fn optional_regular_bytes(path: &Path) -> std::io::Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn periodic_byte_budget(primary: &Path, keep: usize) -> std::io::Result<(u64, u64)> {
    let mut wal = primary.as_os_str().to_os_string();
    wal.push("-wal");
    let generation = optional_regular_bytes(primary)?
        .checked_add(optional_regular_bytes(Path::new(&wal))?)
        .ok_or_else(|| std::io::Error::other("periodic backup size overflow"))?;
    let slots = u64::try_from(keep.max(MIN_KEEP_PER_KIND).saturating_add(3))
        .map_err(|_| std::io::Error::other("periodic backup count overflow"))?;
    let limit = generation
        .checked_mul(slots)
        .ok_or_else(|| std::io::Error::other("periodic backup byte budget overflow"))?
        .max(PERIODIC_BYTES_FLOOR);
    Ok((generation, limit))
}

/// The one admission rule shared by the producer and health reporting.
const fn periodic_limit_reached(
    count: usize,
    bytes: u64,
    projected: u64,
    count_limit: usize,
    byte_limit: u64,
) -> bool {
    count >= count_limit || bytes.saturating_add(projected) > byte_limit
}

/// Publication admission and retention must use the same complete footprint.
/// A planned or refused eviction does not create capacity for the producer.
struct PeriodicPublicationBudget {
    count: usize,
    bytes: u64,
    projected: u64,
    count_limit: usize,
    byte_limit: u64,
}

impl PeriodicPublicationBudget {
    fn new(primary: &Path, keep: usize, count: usize, bytes: u64) -> std::io::Result<Self> {
        let (projected, byte_limit) = periodic_byte_budget(primary, keep)?;
        bytes
            .checked_add(projected)
            .ok_or_else(|| std::io::Error::other("projected backup size overflow"))?;
        Ok(Self {
            count,
            bytes,
            projected,
            // Window + exact publication slot + an older verified pin + one
            // spare publication. Admission needs one unoccupied count slot.
            count_limit: keep.max(MIN_KEEP_PER_KIND).saturating_add(3),
            byte_limit,
        })
    }

    const fn exhausted(&self) -> bool {
        periodic_limit_reached(
            self.count,
            self.bytes,
            self.projected,
            self.count_limit,
            self.byte_limit,
        )
    }

    fn admit(&self) -> std::io::Result<()> {
        if self.exhausted() {
            return Err(std::io::Error::other(PeriodicBudgetExceeded(format!(
                "periodic backup retention budget exhausted ({} images, {} retained bytes, {} projected bytes, {}-byte limit); restore verified snapshot authority or reclaim retained backups before publishing more",
                self.count, self.bytes, self.projected, self.byte_limit
            ))));
        }
        Ok(())
    }

    #[cfg(unix)]
    const fn record_deletion(&mut self, bytes: u64) {
        self.count = self.count.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

#[derive(Default)]
struct HistoricalBackups {
    eligible: Vec<(PathBuf, fs::Metadata)>,
    protected: RotateKindSummary,
    periodic_count: usize,
    bytes: u64,
    failures: Vec<RotationFailure>,
    incomplete: bool,
}

impl HistoricalBackups {
    fn fail(&mut self, path: &Path, error: &std::io::Error) {
        self.incomplete = true;
        self.failures.push(RotationFailure::from_error(path, error));
    }

    fn retained_eligible(&self) -> RotateKindSummary {
        RotateKindSummary {
            kept: self.eligible.len(),
            bytes_kept: self
                .eligible
                .iter()
                .fold(0_u64, |bytes, file| bytes.saturating_add(file.1.len())),
            ..Default::default()
        }
    }
}

/// Native rotation directories supply candidates, not deletion authority. The
/// snapshot guard must prove each candidate's mailbox lineage or exact byte
/// duplication. Recorded doctor/script operations retain undo/resume in full.
fn inspect_historical_backups(storage_root: &Path, primary: &Path) -> HistoricalBackups {
    let mut result = HistoricalBackups::default();
    let root = storage_root.join("doctor/reclaimable");
    for ancestor in [storage_root.join("doctor"), root.clone()] {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return result,
            Ok(_) => {
                result.fail(
                    &ancestor,
                    &std::io::Error::other("reclaimable ancestor is not a real directory"),
                );
                return result;
            }
            Err(error) => {
                result.fail(&ancestor, &error);
                return result;
            }
        }
    }
    let mut pending = vec![(root.clone(), false)];
    let mut visited = 0_usize;
    while let Some((directory, parent_eligible)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                result.fail(&directory, &error);
                continue;
            }
        };
        let mut files = Vec::new();
        let mut directory_complete = true;
        for entry in entries {
            visited = visited.saturating_add(1);
            if visited > MAX_INVENTORY_ENTRIES {
                result.fail(
                    &directory,
                    &std::io::Error::other("reclaimable inventory entry budget exhausted"),
                );
                return result;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    directory_complete = false;
                    result.fail(&directory, &error);
                    continue;
                }
            };
            match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => files.push((entry.path(), metadata)),
                Err(error) => {
                    directory_complete = false;
                    result.fail(&entry.path(), &error);
                }
            }
        }
        // A manifest, receipt, nested directory, or unknown marker can describe
        // an interrupted recovery. Never remove payloads from that transaction.
        let directory_eligible = parent_eligible
            && directory_complete
            && files.iter().all(|(path, metadata)| {
                metadata.file_type().is_file()
                    && primary.file_name().zip(path.file_name()).is_some_and(
                        |(database_name, name)| classify_backup_file(database_name, name).is_some(),
                    )
            });
        for (path, metadata) in files {
            if metadata.file_type().is_dir() {
                let native_rotation =
                    directory == root && path.file_name().is_some_and(is_native_rotation_directory);
                pending.push((path, native_rotation));
                continue;
            }
            result.bytes = result.bytes.saturating_add(metadata.len());
            let periodic = path
                .file_name()
                .is_some_and(|name| is_periodic_backup(primary, name));
            if periodic {
                result.periodic_count = result.periodic_count.saturating_add(1);
            }
            let dated = primary
                .file_name()
                .zip(path.file_name())
                .and_then(|(database_name, name)| {
                    mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(
                        database_name,
                        name,
                    )
                })
                .is_some_and(|candidate| {
                    candidate.kind()
                        == mcp_agent_mail_core::disk::SqliteRecoveryCandidateKind::TimestampedBak
                });
            if directory_eligible && dated {
                result.eligible.push((path, metadata));
            } else {
                result.protected.kept = result.protected.kept.saturating_add(1);
                result.protected.bytes_kept =
                    result.protected.bytes_kept.saturating_add(metadata.len());
            }
        }
    }
    // Oldest first gives each bounded pass deterministic forward progress.
    result.eligible.sort_by(|left, right| left.0.cmp(&right.0));
    result
}

/// Drain empty native batches before the bounded payload inventory. Otherwise
/// a legacy backlog of empty directories can exhaust the inventory limit on
/// every pass and permanently prevent both cleanup and backup publication.
/// The same attempt budget is shared with the post-payload sweep so newly
/// emptied batches do not accumulate in a healthy long-running server.
#[cfg(unix)]
fn reclaim_empty_rotation_directories(
    storage_root: &Path,
    remaining: &mut usize,
    deadline: Option<Instant>,
    report: &mut RotateReport,
    mut admit: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    if *remaining == 0 {
        return Ok(());
    }
    let root = storage_root.join("doctor/reclaimable");
    for ancestor in [storage_root.join("doctor"), root.clone()] {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => {
                return Err(std::io::Error::other(
                    "reclaimable ancestor is not a real directory",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    for entry in fs::read_dir(&root)?.take(MAX_INVENTORY_ENTRIES) {
        let entry = entry?;
        if !is_native_rotation_directory(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_dir() || fs::read_dir(&path)?.next().transpose()?.is_some() {
            continue;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            report.deadline_reached = true;
            break;
        }
        admit()?;
        *remaining -= 1;
        match mcp_agent_mail_db::recovery_retention::delete_empty_reclaim_directory(
            &path, &metadata,
        ) {
            Ok(()) => report.empty_directories_deleted += 1,
            Err(error) => {
                let failure = RotationFailure::from_error(&path, &error);
                let completed = failure.deletion_completed;
                report.failures.push(failure);
                if completed {
                    report.admission_refused = true;
                    break;
                }
            }
        }
        if *remaining == 0 {
            break;
        }
    }
    Ok(())
}

/// Stop the automatic producer when retention cannot make progress. The
/// Count and byte ceilings include resident images and old move-only staging.
/// The byte ceiling scales with the current DB and WAL, with a 512 MiB floor;
/// a shrunken mailbox cannot retain arbitrarily large historical images.
/// Explicit operator exports are unaffected. Never remove evidence merely
/// because verification failed or the operator disabled deletion.
pub(crate) fn admit_periodic_publication(
    storage_root: &Path,
    primary: &Path,
    keep: usize,
) -> std::io::Result<()> {
    periodic_publication_budget(storage_root, primary, keep)?.admit()
}

/// The automatic producer's retained footprint, computed exactly as
/// publication admission computes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodicBackupFootprint {
    /// Strict periodic images (`.bak` and `.bak.<timestamp>`), both beside
    /// the database and under `doctor/reclaimable/`.
    pub images: usize,
    /// Those resident images plus every byte under `doctor/reclaimable/`.
    pub retained_bytes: u64,
    /// Size of the next publication (database plus WAL).
    pub projected_bytes: u64,
    pub image_limit: usize,
    pub byte_limit: u64,
}

impl PeriodicBackupFootprint {
    /// Whether the automatic producer refuses to publish another backup.
    #[must_use]
    pub const fn publication_refused(&self) -> bool {
        periodic_limit_reached(
            self.images,
            self.retained_bytes,
            self.projected_bytes,
            self.image_limit,
            self.byte_limit,
        )
    }
}

/// Read-only view of [`admit_periodic_publication`] for health reporting.
///
/// # Errors
/// Fails closed, like admission, when the inventory is incomplete.
pub fn periodic_backup_footprint(
    storage_root: &Path,
    primary: &Path,
    keep: usize,
) -> std::io::Result<PeriodicBackupFootprint> {
    let budget = periodic_publication_budget(storage_root, primary, keep)?;
    Ok(PeriodicBackupFootprint {
        images: budget.count,
        retained_bytes: budget.bytes,
        projected_bytes: budget.projected,
        image_limit: budget.count_limit,
        byte_limit: budget.byte_limit,
    })
}

fn periodic_publication_budget(
    storage_root: &Path,
    primary: &Path,
    keep: usize,
) -> std::io::Result<PeriodicPublicationBudget> {
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    // Admission is fail-closed, unlike the best-effort health inventory:
    // never discard an unreadable directory entry and call it spare capacity.
    let database_parent = primary
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match fs::read_dir(database_parent) {
        Ok(entries) => {
            for (index, entry) in entries.enumerate() {
                if index >= MAX_INVENTORY_ENTRIES {
                    return Err(std::io::Error::other(
                        "resident backup inventory entry budget exhausted",
                    ));
                }
                let entry = entry?;
                if is_periodic_backup(primary, &entry.file_name()) {
                    let metadata = fs::symlink_metadata(entry.path())?;
                    if !metadata.file_type().is_file() {
                        return Err(std::io::Error::other(
                            "periodic backup entry is not a regular file",
                        ));
                    }
                    count = count.saturating_add(1);
                    bytes = bytes
                        .checked_add(metadata.len())
                        .ok_or_else(|| std::io::Error::other("retained backup size overflow"))?;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let history = inspect_historical_backups(storage_root, primary);
    if history.incomplete {
        return Err(std::io::Error::other(format!(
            "retained backup inventory unavailable; at least {} bytes retained: {}",
            bytes.saturating_add(history.bytes),
            history
                .failures
                .first()
                .map_or("incomplete inventory", |failure| failure.error.as_str()),
        )));
    }
    count = count.saturating_add(history.periodic_count);
    bytes = bytes
        .checked_add(history.bytes)
        .ok_or_else(|| std::io::Error::other("retained backup size overflow"))?;
    PeriodicPublicationBudget::new(primary, keep, count, bytes)
}

#[cfg(unix)]
struct ReclaimBudget {
    attempted: usize,
    bytes: u64,
    byte_limit: u64,
    publication: Option<PeriodicPublicationBudget>,
    deadline: Option<Instant>,
    deadline_reached: bool,
}

#[cfg(unix)]
impl ReclaimBudget {
    fn admit(&mut self, bytes: u64) -> bool {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.deadline_reached = true;
            return false;
        }
        // Admit one oversized image so a large mailbox can still make progress.
        if self.attempted >= MAX_RECLAIM_FILES_PER_PASS
            || (self.attempted > 0 && self.bytes.saturating_add(bytes) > self.byte_limit)
        {
            return false;
        }
        self.attempted = self.attempted.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        true
    }
}

#[cfg(unix)]
fn reclaim_periodic_file(
    primary: &Path,
    file: (&Path, &fs::Metadata),
    guard: &mcp_agent_mail_db::snapshot::SnapshotRetentionGuard,
    budget: &mut ReclaimBudget,
    summary: &mut RotateKindSummary,
    report: &mut RotateReport,
    historical: bool,
) -> bool {
    let (path, metadata) = file;
    if !budget.admit(metadata.len()) {
        summary.kept = summary.kept.saturating_add(1);
        summary.bytes_kept = summary.bytes_kept.saturating_add(metadata.len());
        return false;
    }
    let result = if historical {
        revalidate_historical_directory(primary, path)
            .inspect_err(|_| report.admission_refused = true)
            .and_then(|()| guard.delete_superseded_historical_generation(primary, path, metadata))
    } else {
        guard.delete_superseded_generation(primary, path, metadata)
    };
    match result {
        Ok(bytes) => {
            summary.deleted = summary.deleted.saturating_add(1);
            summary.bytes_deleted = summary.bytes_deleted.saturating_add(bytes.logical_bytes);
            summary.bytes_reclaimed = summary
                .bytes_reclaimed
                .saturating_add(bytes.allocated_bytes.unwrap_or(0));
            if let Some(publication) = budget.publication.as_mut() {
                publication.record_deletion(bytes.logical_bytes);
            }
        }
        Err(error) => {
            let failure = RotationFailure::from_error(path, &error);
            summary.record_failure(&failure, metadata.len());
            report.failures.push(failure);
        }
    }
    true
}

#[cfg(unix)]
struct PendingReclaim {
    path: PathBuf,
    metadata: fs::Metadata,
    historical: bool,
    /// Shorten the normal resident window only while the complete footprint
    /// still prevents publication. The exact slot and verified pin never enter
    /// this queue, and failed deletions never satisfy this condition.
    within_window: bool,
}

#[cfg(unix)]
type ReclaimCursor = (i64, PathBuf);

#[cfg(unix)]
fn reclaim_pending(
    primary: &Path,
    mut pending: Vec<PendingReclaim>,
    guard: &mcp_agent_mail_db::snapshot::SnapshotRetentionGuard,
    budget: &mut ReclaimBudget,
    report: &mut RotateReport,
) {
    // A bounded process-local cursor gives refused candidates and historical
    // files a fair turn. Restarting begins oldest-first again; the durable
    // publication quota/reservation still prevents extra copies after a crash.
    static CURSORS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, ReclaimCursor>>> =
        std::sync::OnceLock::new();
    let cursors = CURSORS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
    let key = |entry: &PendingReclaim| {
        let generation = primary
            .file_name()
            .zip(entry.path.file_name())
            .and_then(|(database, name)| {
                mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(database, name)
            })
            .and_then(mcp_agent_mail_core::disk::SqliteRecoveryCandidateName::generation_micros)
            .unwrap_or(i64::MIN);
        (generation, entry.path.clone())
    };
    pending.sort_by_key(&key);
    let cursor = cursors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(primary)
        .cloned();
    if let Some(cursor) = cursor {
        let start = pending.partition_point(|entry| key(entry) <= cursor);
        pending.rotate_left(start);
    }
    let mut summary = RotateKindSummary::default();
    let mut last_attempted = None;
    let mut stopped = report.admission_refused;
    for entry in pending {
        if stopped
            || (entry.within_window
                && budget
                    .publication
                    .as_ref()
                    .is_none_or(|publication| !publication.exhausted()))
        {
            summary.kept = summary.kept.saturating_add(1);
            summary.bytes_kept = summary.bytes_kept.saturating_add(entry.metadata.len());
            continue;
        }
        if reclaim_periodic_file(
            primary,
            (&entry.path, &entry.metadata),
            guard,
            budget,
            &mut summary,
            report,
            entry.historical,
        ) {
            last_attempted = Some(key(&entry));
        } else {
            // Do not skip an oversized candidate and keep advancing the
            // cursor through smaller files behind it. It needs to be first
            // on the next pass to use the oversized-file allowance.
            stopped = true;
        }
        stopped |= report.admission_refused;
    }
    report.add_kind(BackupKind::ManualBackup.label(), &summary);
    if let Some(cursor) = last_attempted {
        let mut cursors = cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cursors.len() >= 128 && !cursors.contains_key(primary) {
            cursors.pop_first();
        }
        cursors.insert(primary.to_path_buf(), cursor);
    }
}

#[cfg(unix)]
fn revalidate_historical_directory(primary: &Path, file: &Path) -> std::io::Result<()> {
    let parent = file
        .parent()
        .ok_or_else(|| std::io::Error::other("historical backup has no parent"))?;
    if !parent.file_name().is_some_and(is_native_rotation_directory) {
        return Err(std::io::Error::other(
            "historical backup is outside native rotation staging",
        ));
    }
    for (index, entry) in fs::read_dir(parent)?.enumerate() {
        if index >= MAX_INVENTORY_ENTRIES {
            return Err(std::io::Error::other(
                "historical directory revalidation entry budget exhausted",
            ));
        }
        let entry = entry?;
        if !fs::symlink_metadata(entry.path())?.file_type().is_file()
            || primary
                .file_name()
                .is_none_or(|name| classify_backup_file(name, &entry.file_name()).is_none())
        {
            return Err(std::io::Error::other(
                "historical directory acquired recovery markers or unknown material; transaction retained",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_rotation_parent(parent: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::MetadataExt as _;
    let before = fs::symlink_metadata(parent)?;
    if !before.file_type().is_dir() {
        return Err(std::io::Error::other(
            "backup parent is not a real directory",
        ));
    }
    let directory = fs::File::open(parent)?;
    fs2::FileExt::try_lock_exclusive(&directory)?;
    let retained = directory.metadata()?;
    let after = fs::symlink_metadata(parent)?;
    if !after.file_type().is_dir()
        || before.dev() != retained.dev()
        || before.ino() != retained.ino()
        || after.dev() != retained.dev()
        || after.ino() != retained.ino()
    {
        return Err(std::io::Error::other(
            "backup parent changed during rotation admission",
        ));
    }
    Ok(directory)
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

/// Rotate backup files beside `database_path` and reclaim superseded periodic images.
///
/// Keeps `keep_per_kind` newest of each kind and stages the rest into
/// `<storage_root>/doctor/reclaimable/rotation-<UTC ts>[-<n>]/` for operator
/// reclaim. Unix periodic generations instead use verified deletion unless
/// explicitly disabled; historical rotation staging uses that same boundary.
/// Other kinds require `AM_BACKUP_ROTATION_DELETE` opt-in for deletion.
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
/// Snapshot admission spans pin selection through the final eviction. Busy
/// readers/publishers refuse before quarantine creation. Changed metadata or
/// lock ownership stops further moves without losing earlier move accounting.
/// Explicit incident deletion uses the same verification and ownership
/// boundary as periodic deletion. Platforms without that boundary preserve
/// incident evidence; non-Unix staging retains its prior behavior.
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

/// A pass for a caller that cannot wait for a backlog.
///
/// At `budget` it stops reclaiming and leaves the rest to a later pass, with
/// nothing refused. One entry already under way finishes, so the bound is not
/// exact.
pub fn rotate_storage_backups_within(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
    budget: Duration,
) -> std::io::Result<RotateReport> {
    rotate_storage_backups_impl(
        storage_root,
        database_path,
        keep_per_kind,
        stage_backup_noreplace,
        false,
        Instant::now().checked_add(budget),
    )
}

/// The automatic producer already owns the database-parent exporter lease.
/// Reacquiring a second flock would refuse our own admission. Use this entry
/// only inside that existing admitted producer/cleanup closure.
pub(crate) fn rotate_storage_backups_with_exporter_lease(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
) -> std::io::Result<RotateReport> {
    rotate_storage_backups_impl(
        storage_root,
        database_path,
        keep_per_kind,
        stage_backup_noreplace,
        true,
        None,
    )
}

fn rotate_storage_backups_with<F>(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
    stage: F,
) -> std::io::Result<RotateReport>
where
    F: FnMut(&Path, &RotationQuarantine, &fs::Metadata) -> std::io::Result<PathBuf>,
{
    rotate_storage_backups_impl(
        storage_root,
        database_path,
        keep_per_kind,
        stage,
        false,
        None,
    )
}

fn rotate_storage_backups_impl<F>(
    storage_root: &Path,
    database_path: &Path,
    keep_per_kind: usize,
    mut stage: F,
    caller_holds_parent_lease: bool,
    deadline: Option<Instant>,
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

    // Retain the exporter lease before inventory so a cooperating publication
    // cannot make byte accounting or source witnesses stale between the scan
    // and admission. On refusal, still collect the observable retained bytes.
    #[cfg(not(unix))]
    let _ = (caller_holds_parent_lease, deadline);
    #[cfg(unix)]
    let parent_admission = if caller_holds_parent_lease {
        Ok(None)
    } else {
        acquire_rotation_parent(database_parent).map(Some)
    };
    let mut report = RotateReport::default();
    #[cfg(unix)]
    let mut empty_directory_budget = MAX_RECLAIM_DIRECTORIES_PER_PASS;
    #[cfg(unix)]
    let mut early_snapshot_guard = None;
    #[cfg(unix)]
    if parent_admission.is_ok()
        && periodic_reclamation_enabled()
        && let Err(error) = reclaim_empty_rotation_directories(
            &storage_root,
            &mut empty_directory_budget,
            deadline,
            &mut report,
            || {
                if early_snapshot_guard.is_none() {
                    early_snapshot_guard = Some(
                        mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(
                            snapshot_primary,
                        )?,
                    );
                }
                early_snapshot_guard
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("snapshot admission unavailable"))?
                    .revalidate()
            },
        )
    {
        report.admission_refused = true;
        report.failures.push(RotationFailure::from_error(
            &storage_root.join("doctor/reclaimable"),
            &error,
        ));
    }
    let mut history = inspect_historical_backups(&storage_root, &database_path);
    report.inventory_incomplete = history.incomplete;
    report.admission_refused |= history.incomplete;
    report.failures.append(&mut history.failures);
    report.add_kind("protected_quarantine", &history.protected);
    let entries = match fs::read_dir(database_parent) {
        Ok(e) => e,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                report.inventory_incomplete = true;
                report.admission_refused = true;
                report
                    .failures
                    .push(RotationFailure::from_error(database_parent, &error));
            }
            report.add_kind(
                BackupKind::ManualBackup.label(),
                &history.retained_eligible(),
            );
            return Ok(report);
        }
    };

    // Retain the inventory's metadata, not only its size. Staging must not
    // silently adopt a different file subsequently placed at this pathname.
    let mut by_kind: BTreeMap<BackupKind, Vec<(PathBuf, SystemTime, fs::Metadata)>> =
        BTreeMap::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_INVENTORY_ENTRIES {
            report.inventory_incomplete = true;
            report.admission_refused = true;
            report.failures.push(RotationFailure::from_error(
                database_parent,
                &std::io::Error::other("resident backup inventory entry budget exhausted"),
            ));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.inventory_incomplete = true;
                report.admission_refused = true;
                report
                    .failures
                    .push(RotationFailure::from_error(database_parent, &error));
                continue;
            }
        };
        let meta = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) => {
                report.inventory_incomplete = true;
                report.admission_refused = true;
                report
                    .failures
                    .push(RotationFailure::from_error(&entry.path(), &error));
                continue;
            }
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

    // Inventory is only a hint: every move checks its retained file witness.
    // Acquire before choosing a pin, then retain admission for the whole pass.
    // Do not create control state for a missing/empty backup inventory.
    if report.inventory_incomplete || report.admission_refused {
        // An unreadable entry may be the recovery record for a visible payload.
        // Preserve every known file and report lower bounds until the complete
        // namespace can be inspected; never infer absence from an I/O error.
        for (kind, files) in by_kind {
            report.add_kind(
                kind.label(),
                &RotateKindSummary {
                    kept: files.len(),
                    bytes_kept: files
                        .iter()
                        .fold(0_u64, |bytes, file| bytes.saturating_add(file.2.len())),
                    ..Default::default()
                },
            );
        }
        report.add_kind(
            BackupKind::ManualBackup.label(),
            &history.retained_eligible(),
        );
        return Ok(report);
    }
    if by_kind.is_empty() && history.eligible.is_empty() {
        return Ok(report);
    }

    #[cfg(unix)]
    let publication_budget = if periodic_reclamation_enabled() {
        let footprint = by_kind
            .get(&BackupKind::ManualBackup)
            .into_iter()
            .flatten()
            .filter(|(path, _, _)| {
                path.file_name()
                    .is_some_and(|name| is_periodic_backup(snapshot_primary, name))
            })
            .try_fold(
                (history.periodic_count, history.bytes),
                |(count, bytes), (_, _, metadata)| {
                    bytes
                        .checked_add(metadata.len())
                        .map(|bytes| (count.saturating_add(1), bytes))
                        .ok_or_else(|| std::io::Error::other("retained backup size overflow"))
                },
            )
            .and_then(|(count, bytes)| {
                PeriodicPublicationBudget::new(snapshot_primary, keep, count, bytes)
            });
        match footprint {
            Ok(budget) => Some(budget),
            Err(error) => {
                report.admission_refused = true;
                report
                    .failures
                    .push(RotationFailure::from_error(snapshot_primary, &error));
                None
            }
        }
    } else {
        None
    };

    // The guard runs retention after every automatic backup attempt, so on
    // every quick integrity cycle. Selecting the verified pin below re-hashes
    // the backup and runs a full integrity check on it, so a pass that cannot
    // evict anything stops here. Candidates exist only beyond a kind's
    // resident window, in native rotation staging, or inside the periodic
    // window while the publication footprint is exhausted. The exact slot and
    // the pin only remove candidates, so this bound never skips real work.
    #[cfg(unix)]
    let window_may_shorten = publication_budget
        .as_ref()
        .is_some_and(PeriodicPublicationBudget::exhausted);
    #[cfg(not(unix))]
    let window_may_shorten = false;
    if !window_may_shorten
        && history.eligible.is_empty()
        && by_kind.iter().all(|(kind, files)| {
            let periodic_reclaim =
                *kind == BackupKind::ManualBackup && periodic_reclamation_enabled();
            files
                .iter()
                .filter(|(path, _, _)| {
                    occupies_rotation_window(snapshot_primary, path, periodic_reclaim)
                })
                .count()
                <= keep
        })
    {
        for (kind, files) in by_kind {
            let summary = RotateKindSummary {
                kept: files.len(),
                bytes_kept: files
                    .iter()
                    .fold(0_u64, |bytes, file| bytes.saturating_add(file.2.len())),
                ..Default::default()
            };
            report.add_kind(kind.label(), &summary);
        }
        return Ok(report);
    }

    #[cfg(unix)]
    let (_retained_parent, snapshot_guard) = match parent_admission.and_then(|parent| {
        early_snapshot_guard
            .map_or_else(
                || mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(snapshot_primary),
                Ok,
            )
            .map(|guard| (parent, guard))
    }) {
        Ok(guard) => guard,
        Err(error) => {
            // Refusal is observable even when no bytes can be reclaimed. Do
            // not discard the inventory or imply all bytes disappeared.
            report.admission_refused = true;
            report
                .failures
                .push(RotationFailure::from_error(snapshot_primary, &error));
            for (kind, files) in by_kind {
                let summary = RotateKindSummary {
                    kept: files.len(),
                    bytes_kept: files
                        .iter()
                        .fold(0_u64, |bytes, file| bytes.saturating_add(file.2.len())),
                    ..Default::default()
                };
                report.add_kind(kind.label(), &summary);
            }
            report.add_kind(
                BackupKind::ManualBackup.label(),
                &history.retained_eligible(),
            );
            return Ok(report);
        }
    };
    #[cfg(unix)]
    let (snapshot_authority_occupied, pinned_verified_snapshot) = (
        snapshot_guard.metadata_occupied(),
        snapshot_guard.verified_source_path().map(Path::to_path_buf),
    );
    #[cfg(not(unix))]
    let snapshot_authority_occupied = match fs::symlink_metadata(&snapshot_metadata) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    };
    #[cfg(not(unix))]
    let pinned_verified_snapshot = snapshot_authority_occupied
        .then(|| mcp_agent_mail_db::snapshot::verified_snapshot_source_path(snapshot_primary))
        .flatten();

    #[cfg(unix)]
    let mut reclaim_budget = ReclaimBudget {
        attempted: 0,
        bytes: 0,
        byte_limit: optional_regular_bytes(snapshot_primary)
            .unwrap_or(0)
            .max(PERIODIC_BYTES_FLOOR),
        publication: publication_budget,
        deadline,
        deadline_reached: false,
    };
    // Incident verification has its own bounded work allowance. Repeatedly
    // refused incident evidence must not starve the periodic backup queue.
    #[cfg(unix)]
    let mut incident_reclaim_budget = ReclaimBudget {
        attempted: 0,
        bytes: 0,
        byte_limit: reclaim_budget.byte_limit,
        publication: None,
        deadline,
        deadline_reached: false,
    };
    #[cfg(unix)]
    let mut pending_reclaims = Vec::new();

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
                bytes_kept: files
                    .iter()
                    .fold(0_u64, |bytes, file| bytes.saturating_add(file.2.len())),
                ..Default::default()
            };
            warn!(
                metadata = %snapshot_metadata.display(),
                kept = files.len(),
                "verified-snapshot metadata is occupied but no authoritative backup generation can be resolved; refusing manual-backup rotation"
            );
            report.add_kind(kind.label(), &summary);
            report.failures.push(RotationFailure::from_error(
                &snapshot_metadata,
                &std::io::Error::other(
                    "snapshot authority is unresolved; backup generations retained",
                ),
            ));
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
        let periodic_reclaim = kind == BackupKind::ManualBackup && periodic_reclamation_enabled();
        let to_evict = files
            .iter()
            .filter(|(path, _, _)| {
                occupies_rotation_window(snapshot_primary, path, periodic_reclaim)
            })
            .enumerate()
            .filter_map(|(index, file @ (path, _, _))| {
                if path == &mcp_agent_mail_db::snapshot::snapshot_bak_path(snapshot_primary) {
                    return None;
                }
                let is_pinned = kind == BackupKind::ManualBackup
                    && pinned_verified_snapshot.as_ref().is_some_and(|pinned| {
                        fs::canonicalize(path)
                            .is_ok_and(|canonical| canonical.as_path() == pinned.as_path())
                    });
                if is_pinned {
                    return None;
                }
                let within_window = index < keep;
                if within_window && !periodic_reclaim {
                    return None;
                }
                Some((file, within_window))
            })
            .collect::<Vec<_>>();

        let mut summary = RotateKindSummary {
            kept: files.len().saturating_sub(to_evict.len()),
            bytes_kept: files
                .iter()
                .filter(|file| !to_evict.iter().any(|(evicted, _)| evicted.0 == file.0))
                .fold(0_u64, |bytes, file| bytes.saturating_add(file.2.len())),
            ..Default::default()
        };
        for ((path, _mtime, metadata), within_window) in to_evict {
            #[cfg(not(unix))]
            let _ = within_window;
            let size = metadata.len();
            #[cfg(unix)]
            if let Err(error) = snapshot_guard.revalidate() {
                report.admission_refused = true;
                let failure = RotationFailure::from_error(path, &error);
                summary.record_failure(&failure, size);
                report.failures.push(failure);
                continue;
            }
            #[cfg(unix)]
            if kind == BackupKind::ManualBackup && periodic_reclamation_enabled() {
                pending_reclaims.push(PendingReclaim {
                    path: path.clone(),
                    metadata: metadata.clone(),
                    historical: false,
                    within_window,
                });
                continue;
            }
            #[cfg(not(unix))]
            if kind == BackupKind::ManualBackup && periodic_reclamation_enabled() {
                // No descriptor-bound deletion contract on this platform.
                // Publication admission still prevents indefinite growth.
                summary.kept += 1;
                summary.bytes_kept = summary.bytes_kept.saturating_add(size);
                continue;
            }
            if delete_opted_in {
                #[cfg(unix)]
                if !incident_reclaim_budget.admit(size) {
                    summary.kept += 1;
                    summary.bytes_kept = summary.bytes_kept.saturating_add(size);
                    continue;
                }
                #[cfg(unix)]
                let deletion = snapshot_guard
                    .delete_superseded_incident_generation(snapshot_primary, path, metadata)
                    .map(|bytes| (bytes.logical_bytes, bytes.allocated_bytes));
                #[cfg(not(unix))]
                let deletion: std::io::Result<(u64, Option<u64>)> = Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "verified descriptor-bound incident reclamation is unavailable",
                ));
                match deletion {
                    Ok((logical_bytes, allocated_bytes)) => {
                        debug!(kind = kind.label(), path = %path.display(), logical_bytes, "deleted verified rotated backup (explicit opt-in)");
                        summary.deleted += 1;
                        summary.bytes_deleted = summary.bytes_deleted.saturating_add(logical_bytes);
                        summary.bytes_reclaimed = summary
                            .bytes_reclaimed
                            .saturating_add(allocated_bytes.unwrap_or(0));
                    }
                    Err(err) => {
                        warn!(
                            kind = kind.label(),
                            path = %path.display(),
                            %err,
                            "incident reclamation refused or durability unconfirmed"
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
                    #[cfg(unix)]
                    if let Err(error) = snapshot_guard.revalidate() {
                        report.admission_refused = true;
                        let failure = RotationFailure {
                            source: path.clone(),
                            rename_completed: true,
                            deletion_completed: false,
                            requested_destination: Some(dest),
                            error: format!(
                                "backup moved but snapshot authority changed: {error}; evidence retained, do not retry"
                            ),
                        };
                        summary.record_failure(&failure, size);
                        report.failures.push(failure);
                        continue;
                    }
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

        report.add_kind(kind.label(), &summary);
    }

    let mut historical_summary = RotateKindSummary::default();
    for (path, metadata) in history.eligible {
        #[cfg(unix)]
        if periodic_reclamation_enabled() && pinned_verified_snapshot.is_some() {
            pending_reclaims.push(PendingReclaim {
                path,
                metadata,
                historical: true,
                within_window: false,
            });
            continue;
        }
        historical_summary.kept = historical_summary.kept.saturating_add(1);
        historical_summary.bytes_kept =
            historical_summary.bytes_kept.saturating_add(metadata.len());
    }
    report.add_kind(BackupKind::ManualBackup.label(), &historical_summary);
    #[cfg(unix)]
    reclaim_pending(
        snapshot_primary,
        pending_reclaims,
        &snapshot_guard,
        &mut reclaim_budget,
        &mut report,
    );
    #[cfg(unix)]
    {
        report.deadline_reached |=
            reclaim_budget.deadline_reached || incident_reclaim_budget.deadline_reached;
    }
    #[cfg(unix)]
    if !report.admission_refused
        && periodic_reclamation_enabled()
        && let Err(error) = reclaim_empty_rotation_directories(
            &storage_root,
            &mut empty_directory_budget,
            deadline,
            &mut report,
            || snapshot_guard.revalidate(),
        )
    {
        report.admission_refused = true;
        report.failures.push(RotationFailure::from_error(
            &storage_root.join("doctor/reclaimable"),
            &error,
        ));
    }
    #[cfg(unix)]
    if !report.admission_refused
        && let Err(error) = snapshot_guard.revalidate()
    {
        report.admission_refused = true;
        report
            .failures
            .push(RotationFailure::from_error(snapshot_primary, &error));
    }

    if report.evicted() > 0
        || report.empty_directories_deleted > 0
        || report.unconfirmed_moves > 0
        || report.deadline_reached
        || !report.failures.is_empty()
    {
        info!(
            empty_directories_deleted = report.empty_directories_deleted,
            staged = report.staged,
            deleted = report.deleted,
            kept = report.kept,
            bytes_staged = report.bytes_staged,
            bytes_deleted = report.bytes_deleted,
            bytes_reclaimed = report.bytes_reclaimed,
            bytes_retained = report.bytes_retained(),
            inventory_incomplete = report.inventory_incomplete,
            admission_refused = report.admission_refused,
            deadline_reached = report.deadline_reached,
            failures = report.failures.len(),
            unconfirmed_deletions = report.unconfirmed_deletions,
            bytes_unconfirmed_deletions = report.bytes_unconfirmed_deletions,
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

    #[cfg(unix)]
    fn verified_mailbox(root: &Path) -> PathBuf {
        let primary = root.join("storage.sqlite3");
        let conn =
            mcp_agent_mail_db::CanonicalDbConn::open_file(primary.to_str().unwrap()).unwrap();
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .unwrap();
        conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[])
            .unwrap();
        drop(conn);
        fs::copy(
            &primary,
            mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary),
        )
        .unwrap();
        mcp_agent_mail_db::snapshot::record_snapshot_metadata(&primary, 2_000_000_000_000_000)
            .unwrap();
        primary
    }

    #[cfg(unix)]
    fn names_on_disk(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = walkdir::WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string()
            })
            .filter(|name| Path::new(name).extension().is_none_or(|ext| ext != "lock"))
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mailbox_with_backlog(root: &Path) -> PathBuf {
        let primary = verified_mailbox(root);
        for day in 1..=9 {
            fs::copy(
                &primary,
                root.join(format!("storage.sqlite3.bak.2000010{day}_000000")),
            )
            .unwrap();
        }
        primary
    }

    #[cfg(unix)]
    #[test]
    fn spent_budget_leaves_the_backlog_for_the_next_pass() {
        let control = TempDir::new().unwrap();
        let control_primary = mailbox_with_backlog(control.path());
        let unbudgeted = rotate_storage_backups(control.path(), &control_primary, 3).unwrap();
        assert!(unbudgeted.deleted > 0);

        let tmp = TempDir::new().unwrap();
        let primary = mailbox_with_backlog(tmp.path());
        let before = names_on_disk(tmp.path());
        let spent = rotate_storage_backups_within(tmp.path(), &primary, 3, Duration::ZERO).unwrap();
        assert!(spent.deadline_reached);
        assert_eq!(spent.deleted, 0);
        assert!(spent.failures.is_empty(), "{:?}", spent.failures);
        assert!(!spent.admission_refused);
        assert_eq!(
            spent.kept,
            unbudgeted.kept + unbudgeted.deleted,
            "a spent budget counts what it left as kept"
        );
        assert_eq!(names_on_disk(tmp.path()), before);

        // The background pass has no budget: same evictions, same kept set.
        let next = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
        assert!(!next.deadline_reached);
        assert_eq!(next.deleted, unbudgeted.deleted);
        assert_eq!(next.kept, unbudgeted.kept);
        assert_eq!(
            names_on_disk(tmp.path()),
            names_on_disk(control.path()),
            "the kept set must not depend on which pass reclaimed the backlog"
        );
        assert!(mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn budget_not_reached_changes_nothing() {
        let control = TempDir::new().unwrap();
        let control_primary = mailbox_with_backlog(control.path());
        let unbudgeted = rotate_storage_backups(control.path(), &control_primary, 3).unwrap();

        let tmp = TempDir::new().unwrap();
        let primary = mailbox_with_backlog(tmp.path());
        let ample = Duration::from_secs(3600);
        let report = rotate_storage_backups_within(tmp.path(), &primary, 3, ample).unwrap();
        assert!(!report.deadline_reached);
        assert_eq!(report.deleted, unbudgeted.deleted);
        assert_eq!(report.kept, unbudgeted.kept);
        assert_eq!(names_on_disk(tmp.path()), names_on_disk(control.path()));
    }

    #[cfg(unix)]
    #[test]
    fn spent_budget_leaves_empty_rotation_directories_for_the_next_pass() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let empties = [1, 2, 3].map(|n| {
            let directory = tmp
                .path()
                .join(format!("doctor/reclaimable/rotation-1999122{n}T235959Z"));
            fs::create_dir_all(&directory).unwrap();
            directory
        });

        let spent = rotate_storage_backups_within(tmp.path(), &primary, 3, Duration::ZERO).unwrap();
        assert!(spent.deadline_reached);
        assert_eq!(spent.empty_directories_deleted, 0);
        assert!(spent.failures.is_empty(), "{:?}", spent.failures);
        assert!(empties.iter().all(|directory| directory.is_dir()));

        let next = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
        assert!(!next.deadline_reached);
        assert_eq!(next.empty_directories_deleted, 3);
        assert!(empties.iter().all(|directory| !directory.exists()));
    }

    #[cfg(unix)]
    #[test]
    fn periodic_cycles_reclaim_disk_and_historical_staging() {
        use std::os::unix::fs::MetadataExt as _;
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let staged = tmp
            .path()
            .join("doctor/reclaimable/rotation-19991231T235959Z");
        fs::create_dir_all(&staged).unwrap();
        fs::copy(&primary, staged.join("storage.sqlite3.bak.20000101_000000")).unwrap();
        fs::write(
            staged.join("storage.sqlite3.corrupt-evidence"),
            b"incident evidence",
        )
        .unwrap();
        let live = fs::read(&primary).unwrap();
        let generation_blocks = fs::metadata(&primary).unwrap().blocks();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || {
                // 30 days of hourly generations without sleeps or a mocked FS.
                for cycle in 0..720 {
                    admit_periodic_publication(tmp.path(), &primary, 3).unwrap();
                    let day = cycle / 24 + 1;
                    let hour = cycle % 24;
                    let generation = tmp
                        .path()
                        .join(format!("storage.sqlite3.bak.200001{day:02}_{hour:02}0000"));
                    fs::copy(&primary, &generation).unwrap();
                    let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
                    assert!(report.failures.is_empty(), "{:?}", report.failures);
                    let inventory = inspect_storage_backups(&primary).unwrap();
                    assert!(inventory.artifact_count <= 4); // window plus exact pin
                    let blocks: u64 = inventory
                        .artifacts
                        .iter()
                        .map(|artifact| fs::metadata(&artifact.path).unwrap().blocks())
                        .sum();
                    assert!(
                        blocks
                            <= fs::metadata(mcp_agent_mail_db::snapshot::snapshot_bak_path(
                                &primary
                            ))
                            .unwrap()
                            .blocks()
                                + 3 * generation_blocks
                    );
                    assert_eq!(report.bytes_unconfirmed_deletions, 0);
                    assert_eq!(
                        report.bytes_retained(),
                        inventory
                            .artifacts
                            .iter()
                            .map(|artifact| artifact.bytes)
                            .sum::<u64>()
                            + 17
                    );
                }
            },
        );
        assert!(!staged.join("storage.sqlite3.bak.20000101_000000").exists());
        assert_eq!(
            fs::read(staged.join("storage.sqlite3.corrupt-evidence")).unwrap(),
            b"incident evidence"
        );
        assert_eq!(fs::read(primary).unwrap(), live);
    }

    #[test]
    fn publication_budget_counts_previously_staged_material() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        let staged = tmp.path().join("doctor/reclaimable/rotation-old");
        fs::create_dir_all(&staged).unwrap();
        for index in 0..6 {
            fs::write(
                staged.join(format!("storage.sqlite3.bak.20000101_00000{index}")),
                b"retained",
            )
            .unwrap();
        }
        assert!(admit_periodic_publication(tmp.path(), &primary, 3).is_err());
        assert_eq!(fs::read_dir(staged).unwrap().count(), 6);
    }

    #[test]
    fn byte_budget_refuses_large_history_below_the_count_limit() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        fs::write(&primary, b"live size estimate").unwrap();
        let history = tmp
            .path()
            .join("doctor/reclaimable/rotated-backups-recorded");
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join("manifest.json"), b"operator restore authority").unwrap();
        let backup = history.join("storage.sqlite3.bak.20000101_000000");
        let file = fs::File::create(&backup).unwrap();
        // A sparse fixture exercises logical-byte admission without consuming
        // half a GiB in the test filesystem.
        file.set_len(PERIODIC_BYTES_FLOOR + 1).unwrap();
        drop(file);
        let error = admit_periodic_publication(tmp.path(), &primary, 3).unwrap_err();
        assert!(periodic_publication_budget_exhausted(&error));
        assert!(error.to_string().contains("1 images"));
        assert!(error.to_string().contains("retained bytes"));
        assert_eq!(
            fs::metadata(backup).unwrap().len(),
            PERIODIC_BYTES_FLOOR + 1
        );
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn reclamation_shortens_the_window_using_confirmed_aggregate_capacity() {
        use std::io::Write as _;
        use std::os::unix::fs::MetadataExt as _;

        for failed_oldest in [false, true] {
            let tmp = TempDir::new().unwrap();
            let primary = verified_mailbox(tmp.path());
            let live = fs::read(&primary).unwrap();
            let generation_bytes = u64::try_from(live.len()).unwrap();
            let oldest = tmp.path().join("storage.sqlite3.bak.20000101_000000");
            let newer = tmp.path().join("storage.sqlite3.bak.20000102_000000");
            fs::copy(&primary, &oldest).unwrap();
            fs::copy(&primary, &newer).unwrap();
            if failed_oldest {
                fs::OpenOptions::new()
                    .write(true)
                    .open(&oldest)
                    .unwrap()
                    .write_all(b"interrupted export")
                    .unwrap();
            }
            let oldest_bytes = fs::read(&oldest).unwrap();
            let valid_blocks = fs::metadata(if failed_oldest { &newer } else { &oldest })
                .unwrap()
                .blocks();

            let history = tmp
                .path()
                .join("doctor/reclaimable/rotated-backups-recorded");
            fs::create_dir_all(&history).unwrap();
            let manifest = history.join("manifest.json");
            let manifest_bytes = b"operator restore authority";
            fs::write(&manifest, manifest_bytes).unwrap();
            let evidence = history.join("recovery-evidence.bin");
            let evidence_bytes = PERIODIC_BYTES_FLOOR
                - 3 * generation_bytes
                - u64::try_from(manifest_bytes.len()).unwrap();
            fs::File::create(&evidence)
                .unwrap()
                .set_len(evidence_bytes)
                .unwrap();
            let evidence_inode = fs::metadata(&evidence).unwrap().ino();

            // Resident bytes alone are small. The complete retained footprint
            // exactly fills the byte ceiling, leaving no room for the next
            // publication; count-based rotation would select no candidates.
            let error = admit_periodic_publication(tmp.path(), &primary, 3).unwrap_err();
            assert!(periodic_publication_budget_exhausted(&error));
            assert!(error.to_string().contains("3 images"));
            let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("AM_BACKUP_ROTATION_DELETE", "true")],
                || rotate_storage_backups(tmp.path(), &primary, 3).unwrap(),
            );
            assert_eq!(report.deleted, 1);
            assert_eq!(report.bytes_deleted, generation_bytes);
            assert_eq!(report.bytes_reclaimed, valid_blocks * 512);
            assert_eq!(
                report.bytes_retained(),
                PERIODIC_BYTES_FLOOR - generation_bytes
            );
            assert_eq!(report.unconfirmed_deletions, 0);
            assert!(!report.admission_refused);
            if failed_oldest {
                // A failed candidate cannot be subtracted from the quota or
                // indefinitely starve a later valid generation in the window.
                assert_eq!(fs::read(&oldest).unwrap(), oldest_bytes);
                assert!(!newer.exists());
                assert_eq!(report.failures.len(), 1);
                assert_eq!(report.failures[0].source, oldest);
            } else {
                assert!(!oldest.exists());
                assert_eq!(fs::read(&newer).unwrap(), live);
                assert_eq!(report.failures.len(), 0);
            }
            admit_periodic_publication(tmp.path(), &primary, 3).unwrap();
            let next = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("AM_BACKUP_ROTATION_DELETE", "true")],
                || rotate_storage_backups(tmp.path(), &primary, 3).unwrap(),
            );
            assert_eq!(
                next.deleted, 0,
                "restored capacity keeps the remaining window"
            );
            assert!(next.failures.is_empty(), "{:?}", next.failures);
            assert_eq!(next.bytes_retained(), report.bytes_retained());
            assert_eq!(fs::read(&manifest).unwrap(), manifest_bytes);
            let retained_evidence = fs::metadata(&evidence).unwrap();
            assert_eq!(retained_evidence.len(), evidence_bytes);
            assert_eq!(retained_evidence.ino(), evidence_inode);
            assert_eq!(fs::read(&primary).unwrap(), live);
            assert_eq!(
                fs::read(mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary)).unwrap(),
                live
            );
            assert!(mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn protected_history_count_can_shorten_the_resident_window() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let generation_bytes = fs::metadata(&primary).unwrap().len();
        let oldest = tmp.path().join("storage.sqlite3.bak.20000101_000000");
        let newer = tmp.path().join("storage.sqlite3.bak.20000102_000000");
        fs::copy(&primary, &oldest).unwrap();
        fs::copy(&primary, &newer).unwrap();
        let history = tmp
            .path()
            .join("doctor/reclaimable/rotated-backups-recorded");
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join("manifest.json"), b"restore authority").unwrap();
        for day in 1..=3 {
            fs::copy(
                &primary,
                history.join(format!("storage.sqlite3.bak.1999120{day}_000000")),
            )
            .unwrap();
        }
        let error = admit_periodic_publication(tmp.path(), &primary, 3).unwrap_err();
        assert!(periodic_publication_budget_exhausted(&error));
        assert!(error.to_string().contains("6 images"));
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || rotate_storage_backups(tmp.path(), &primary, 3).unwrap(),
        );
        assert_eq!(report.deleted, 1);
        assert_eq!(report.bytes_deleted, generation_bytes);
        assert_eq!(report.bytes_retained(), 5 * generation_bytes + 17);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!oldest.exists());
        assert!(newer.exists());
        assert_eq!(fs::read_dir(&history).unwrap().count(), 4);
        admit_periodic_publication(tmp.path(), &primary, 3).unwrap();
        assert!(mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some());
    }

    #[test]
    fn manual_and_sidecar_backups_do_not_consume_periodic_count_slots() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        for index in 0..12 {
            fs::write(
                tmp.path()
                    .join(format!("storage.sqlite3.manual-backup-{index}")),
                b"manual",
            )
            .unwrap();
            fs::write(
                tmp.path()
                    .join(format!("storage.sqlite3-wal.bak.20000101_0000{index:02}")),
                b"wal",
            )
            .unwrap();
        }
        admit_periodic_publication(tmp.path(), &primary, 3).unwrap();
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 24);
    }

    #[cfg(unix)]
    #[test]
    fn recorded_and_interrupted_quarantines_remain_recoverable_and_accounted() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let generation_bytes = fs::metadata(&primary).unwrap().len();
        let mut expected_retained = generation_bytes;
        for (name, marker) in [
            ("rotation-20000101T000000Z", "actions.jsonl"),
            ("rotated-backups-script", "manifest.json"),
            ("proactive-staging-script", "manifest.json"),
        ] {
            let directory = tmp.path().join("doctor/reclaimable").join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::copy(
                &primary,
                directory.join("storage.sqlite3.bak.20000101_000000"),
            )
            .unwrap();
            fs::write(directory.join(marker), b"restore/resume authority").unwrap();
            expected_retained += generation_bytes + 24;
        }
        let completed = tmp
            .path()
            .join("doctor/reclaimable/rotation-19991231T235959Z");
        fs::create_dir_all(&completed).unwrap();
        let retired = completed.join("storage.sqlite3.bak.20000101_000000");
        fs::copy(&primary, &retired).unwrap();
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || rotate_storage_backups(tmp.path(), &primary, 3).unwrap(),
        );
        assert_eq!(report.deleted, 1);
        assert_eq!(report.bytes_deleted, generation_bytes);
        assert_eq!(report.bytes_retained(), expected_retained);
        assert_eq!(report.per_kind["protected_quarantine"].kept, 6);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!retired.exists());
        for name in [
            "rotation-20000101T000000Z",
            "rotated-backups-script",
            "proactive-staging-script",
        ] {
            assert_eq!(
                fs::read(
                    tmp.path()
                        .join("doctor/reclaimable")
                        .join(name)
                        .join("storage.sqlite3.bak.20000101_000000")
                )
                .unwrap(),
                fs::read(&primary).unwrap()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_historical_verification_preserves_evidence_and_completed_accounting() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let directory = tmp
            .path()
            .join("doctor/reclaimable/rotation-19991231T235959Z");
        fs::create_dir_all(&directory).unwrap();
        fs::copy(
            &primary,
            directory.join("storage.sqlite3.bak.20000101_000000"),
        )
        .unwrap();
        let invalid = directory.join("storage.sqlite3.bak.20000102_000000");
        fs::write(&invalid, b"failed backup verification").unwrap();
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || rotate_storage_backups(tmp.path(), &primary, 3).unwrap(),
        );
        assert_eq!(report.deleted, 1);
        assert_eq!(report.bytes_deleted, fs::metadata(&primary).unwrap().len());
        assert_eq!(
            report.bytes_retained(),
            fs::metadata(&primary).unwrap().len() + 26
        );
        assert_eq!(report.failures.len(), 1);
        assert!(!report.failures[0].deletion_completed);
        assert_eq!(fs::read(invalid).unwrap(), b"failed backup verification");
        assert!(mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_quarantine_inventory_is_not_a_repairable_budget_refusal() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("doctor")).unwrap();
        let error = admit_periodic_publication(tmp.path(), &primary, 3).unwrap_err();
        assert!(!periodic_publication_budget_exhausted(&error));
        let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
        assert!(report.inventory_incomplete);
        assert!(report.admission_refused);
        assert_eq!(report.bytes_reclaimed, 0);
        assert_ne!(report.failures.len(), 0);
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_cleanup_advances_past_a_repeatedly_refused_generation() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let invalid = tmp.path().join("storage.sqlite3.bak.20000101_000000");
        let valid = tmp.path().join("storage.sqlite3.bak.20000102_000000");
        fs::write(&invalid, b"incomplete export evidence").unwrap();
        fs::copy(&primary, &valid).unwrap();
        let guard = mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(&primary).unwrap();
        for pass in 0..2 {
            let pending = [&invalid, &valid]
                .into_iter()
                .map(|path| PendingReclaim {
                    path: path.clone(),
                    metadata: fs::metadata(path).unwrap(),
                    historical: false,
                    within_window: false,
                })
                .collect();
            // A zero byte allowance still admits one oversized candidate.
            // Repeated refusal of that candidate must not starve the next one.
            let mut budget = ReclaimBudget {
                attempted: 0,
                bytes: 0,
                byte_limit: 0,
                publication: None,
                deadline: None,
                deadline_reached: false,
            };
            let mut report = RotateReport::default();
            reclaim_pending(&primary, pending, &guard, &mut budget, &mut report);
            assert_eq!(budget.attempted, 1);
            assert_eq!(report.deleted, pass);
            assert_eq!(fs::read(&invalid).unwrap(), b"incomplete export evidence");
            assert_eq!(
                report.bytes_retained(),
                fs::metadata(&invalid).unwrap().len()
                    + if pass == 0 {
                        fs::metadata(&valid).unwrap().len()
                    } else {
                        0
                    }
            );
        }
        assert!(!valid.exists());
        drop(guard);
        assert!(mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn oversized_candidate_gets_a_turn_before_smaller_later_files() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let paths = [1, 2, 3].map(|day| {
            tmp.path()
                .join(format!("storage.sqlite3.bak.2000010{day}_000000"))
        });
        for (path, bytes) in paths.iter().zip([b"a".as_slice(), b"big", b"c"]) {
            fs::write(path, bytes).unwrap();
        }
        let guard = mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(&primary).unwrap();
        for expected in &paths[..2] {
            let pending = paths
                .iter()
                .map(|path| PendingReclaim {
                    path: path.clone(),
                    metadata: fs::metadata(path).unwrap(),
                    historical: false,
                    within_window: false,
                })
                .collect();
            let mut budget = ReclaimBudget {
                attempted: 0,
                bytes: 0,
                byte_limit: 2,
                publication: None,
                deadline: None,
                deadline_reached: false,
            };
            let mut report = RotateReport::default();
            reclaim_pending(&primary, pending, &guard, &mut budget, &mut report);
            assert_eq!(budget.attempted, 1);
            assert_eq!(report.failures.len(), 1);
            assert_eq!(&report.failures[0].source, expected);
            assert_eq!(report.bytes_retained(), 5);
            assert_eq!(report.deleted, 0, "invalid fixtures remain recoverable");
        }
    }

    #[cfg(unix)]
    #[test]
    fn exporter_parent_lease_refuses_public_rotation_without_losing_inventory() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        for day in 1..=3 {
            fs::write(
                tmp.path()
                    .join(format!("storage.sqlite3.corrupt-2000010{day}")),
                b"evidence",
            )
            .unwrap();
        }
        let exporter = fs::File::open(tmp.path()).unwrap();
        fs2::FileExt::try_lock_exclusive(&exporter).unwrap();
        let report = rotate_with_delete_off(tmp.path(), 1);
        assert!(report.admission_refused);
        assert_eq!(report.kept, 3);
        assert_eq!(report.bytes_retained(), 24);
        assert_eq!(report.evicted(), 0);
        assert_eq!(report.bytes_reclaimed, 0);
        assert!(!tmp.path().join("doctor").exists());
        assert!(
            !primary
                .with_file_name("storage.sqlite3.bak.authority.lock")
                .exists()
        );
        drop(exporter);
        let resumed = rotate_with_delete_off(tmp.path(), 1);
        assert!(!resumed.admission_refused);
        assert!(resumed.failures.is_empty(), "{:?}", resumed.failures);
        assert_eq!(resumed.staged, 2);
        assert_eq!(resumed.bytes_retained(), 24);
    }

    #[cfg(unix)]
    #[test]
    fn late_recovery_manifest_revokes_historical_candidate_admission() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        let directory = tmp
            .path()
            .join("doctor/reclaimable/rotation-20000101T000000Z");
        fs::create_dir_all(&directory).unwrap();
        let backup = directory.join("storage.sqlite3.bak.20000101_000000");
        fs::write(&backup, b"recoverable transaction payload").unwrap();
        assert_eq!(
            inspect_historical_backups(tmp.path(), &primary)
                .eligible
                .len(),
            1
        );
        revalidate_historical_directory(&primary, &backup).unwrap();
        fs::write(directory.join("manifest.json"), b"restore authority").unwrap();
        assert!(revalidate_historical_directory(&primary, &backup).is_err());
        let history = inspect_historical_backups(tmp.path(), &primary);
        assert!(history.eligible.is_empty());
        assert_eq!(history.protected.kept, 2);
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"recoverable transaction payload"
        );
    }

    #[test]
    fn no_delete_cycles_stop_publication_without_destroying_evidence() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        fs::write(
            mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary),
            b"current",
        )
        .unwrap();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || {
                // Each completed iteration publishes exactly one generation, so
                // `cycle` is also the number of publications admitted so far.
                for cycle in 0..720 {
                    if admit_periodic_publication(tmp.path(), &primary, 3).is_err() {
                        assert_eq!(cycle, 5);
                        return;
                    }
                    fs::write(
                        tmp.path()
                            .join(format!("storage.sqlite3.bak.20000101_0000{cycle:02}")),
                        b"retained",
                    )
                    .unwrap();
                    let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
                    assert_eq!(report.deleted, 0);
                    assert_eq!(report.failures.len(), 0);
                }
                panic!("move-only rotation must not permit endless publication");
            },
        );
        assert_eq!(
            fs::read(mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary)).unwrap(),
            b"current"
        );
    }

    #[cfg(unix)]
    #[test]
    fn periodic_reclamation_refuses_busy_authority_and_unverified_replacement() {
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let old = tmp.path().join("storage.sqlite3.bak.20000101_000000");
        fs::write(&old, b"old recovery evidence").unwrap();
        let guard = mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(&primary).unwrap();
        std::thread::scope(|scope| {
            assert!(
                scope
                    .spawn(|| {
                        let report = rotate_storage_backups(tmp.path(), &primary, 1).unwrap();
                        assert_eq!(report.deleted, 0);
                        assert!(report.bytes_retained() >= 21);
                        !report.failures.is_empty()
                    })
                    .join()
                    .unwrap()
            );
        });
        // External corruption of the pinned bytes must invalidate reclamation,
        // even when the metadata sidecar and admission lock have not changed.
        fs::write(
            mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary),
            b"corrupt",
        )
        .unwrap();
        assert!(
            guard
                .delete_superseded_generation(&primary, &old, &fs::metadata(&old).unwrap())
                .is_err()
        );
        assert_eq!(fs::read(old).unwrap(), b"old recovery evidence");
    }

    #[cfg(unix)]
    #[test]
    fn retention_with_nothing_to_evict_leaves_snapshot_authority_alone() {
        // Retention follows every automatic backup attempt, so it runs on each
        // quick guard cycle. When the inventory fits the resident window, a pass
        // must not take exclusive snapshot authority: selecting the pin
        // re-verifies the whole backup, and is refused while another holder
        // (a restore, a publisher, or another retention pass) has it.
        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let previous = tmp.path().join("storage.sqlite3.bak.20000101_000000");
        fs::copy(&primary, &previous).unwrap();
        let rotate_while_held = |keep: usize| {
            let holder =
                mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(&primary).unwrap();
            let report = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                            &[],
                            || rotate_storage_backups(tmp.path(), &primary, keep).unwrap(),
                        )
                    })
                    .join()
                    .unwrap()
            });
            drop(holder);
            report
        };
        let idle = rotate_while_held(3);
        assert!(!idle.admission_refused, "{:?}", idle.failures);
        assert!(idle.failures.is_empty(), "{:?}", idle.failures);
        assert_eq!(idle.kept, 2);
        assert_eq!(idle.evicted(), 0);
        // A real candidate still needs authority, so it waits for the holder.
        let contended = rotate_while_held(1);
        assert!(contended.admission_refused);
        assert_eq!(contended.evicted(), 0);
        assert!(previous.exists());
    }

    #[cfg(unix)]
    #[test]
    fn periodic_reclamation_leaves_operator_exports_and_wal_copies_in_place() {
        // Only strict periodic generations are reclaimable. Under the default
        // policy and the delete opt-in, manual exports and WAL backup copies
        // beyond the window are neither refused deletions on every pass nor
        // moved into staging, where they would count against the budget.
        let policies: [&[(&str, &str)]; 2] = [&[], &[("AM_BACKUP_ROTATION_DELETE", "true")]];
        for policy in policies {
            let tmp = TempDir::new().unwrap();
            let primary = verified_mailbox(tmp.path());
            let mut operator_files = Vec::new();
            for day in 1..=4 {
                for name in [
                    format!("storage.sqlite3.manual-backup-2000010{day}_000000"),
                    format!("storage.sqlite3-wal.bak.2000010{day}_000000"),
                ] {
                    let path = tmp.path().join(name);
                    fs::write(&path, b"operator evidence").unwrap();
                    operator_files.push(path);
                }
            }
            let rotate = |keep: usize| {
                mcp_agent_mail_core::config::with_process_env_overrides_for_test(policy, || {
                    rotate_storage_backups(tmp.path(), &primary, keep).unwrap()
                })
            };
            // The exact slot is the only periodic generation, so these files
            // do not fill the one-slot window and the pass has no candidates.
            let idle = rotate(1);
            assert!(idle.failures.is_empty(), "{policy:?}: {:?}", idle.failures);
            assert!(!idle.admission_refused, "{policy:?}");
            assert_eq!(idle.evicted(), 0, "{policy:?}");
            // A superseded periodic generation makes the pass run in full; the
            // operator files are still neither attempted nor moved.
            fs::copy(
                &primary,
                tmp.path().join("storage.sqlite3.bak.20000101_000000"),
            )
            .unwrap();
            let full = rotate(1);
            assert!(
                full.failures
                    .iter()
                    .all(|failure| !operator_files.contains(&failure.source)),
                "{policy:?}: {:?}",
                full.failures
            );
            for path in &operator_files {
                assert_eq!(fs::read(path).unwrap(), b"operator evidence", "{policy:?}");
            }
            assert!(!tmp.path().join("doctor").exists(), "{policy:?}");
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn rotation_coordination_preserves_partial_progress_when_authority_changes() {
        for replace_lock in [false, true] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let primary = root.join("storage.sqlite3");
            for day in 1..=3 {
                fs::write(
                    root.join(format!("storage.sqlite3.bak.2026010{day}_000000")),
                    b"retained backup contents",
                )
                .unwrap();
            }
            let metadata_path = mcp_agent_mail_db::snapshot::snapshot_meta_path(&primary);
            let lock_path = root.join("storage.sqlite3.bak.authority.lock");
            let mut moves = Vec::new();
            let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("AM_BACKUP_ROTATION_DELETE", "0")],
                || {
                    rotate_storage_backups_with(
                        &root,
                        &primary,
                        1,
                        |source, directory, metadata| {
                            let destination = stage_backup_noreplace(source, directory, metadata)?;
                            moves.push((source.to_path_buf(), destination.clone()));
                            // Model an external actor that does not honor admission.
                            if replace_lock {
                                fs::rename(&lock_path, root.join("retained-authority.lock"))?;
                                fs::write(&lock_path, b"replacement authority evidence")?;
                            } else {
                                fs::write(&metadata_path, b"changed snapshot evidence")?;
                            }
                            Ok(destination)
                        },
                    )
                    .unwrap()
                },
            );
            assert_eq!(moves.len(), 1, "stale admission authorized a second move");
            assert_eq!(
                (
                    report.staged,
                    report.deleted,
                    report.kept,
                    report.unconfirmed_moves
                ),
                (0, 0, 2, 1)
            );
            assert_eq!(
                report.bytes_unconfirmed_moves,
                b"retained backup contents".len() as u64
            );
            assert_eq!(report.failures.len(), 2);
            assert!(report.failures[0].rename_completed);
            assert!(!report.failures[1].rename_completed);
            assert_eq!(
                report.failures[0].requested_destination.as_ref(),
                Some(&moves[0].1)
            );
            assert!(!moves[0].0.exists());
            assert_eq!(fs::read(&moves[0].1).unwrap(), b"retained backup contents");
            for day in [1, 3] {
                assert_eq!(
                    fs::read(root.join(format!("storage.sqlite3.bak.2026010{day}_000000")))
                        .unwrap(),
                    b"retained backup contents"
                );
            }
            let evidence = if replace_lock {
                &lock_path
            } else {
                &metadata_path
            };
            assert_ne!(fs::read(evidence).unwrap().len(), 0);
            if replace_lock {
                assert!(root.join("retained-authority.lock").is_file());
            } else {
                let resumed = rotate_with_delete_off(&root, 1);
                assert_eq!((resumed.kept, resumed.staged), (3, 0));
                assert_eq!(
                    fs::read(&metadata_path).unwrap(),
                    b"changed snapshot evidence"
                );
            }
        }
    }

    #[test]
    fn rotation_coordination_empty_inventory_does_not_create_authority() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join("unrelated.txt"), b"untouched").unwrap();
        assert_eq!(rotate_with_delete_off(&root, 1), RotateReport::default());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(fs::read(root.join("unrelated.txt")).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    #[test]
    fn rotation_coordination_excludes_metadata_refresh_through_final_move() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let primary = root.join("storage.sqlite3");
        let conn =
            mcp_agent_mail_db::CanonicalDbConn::open_file(primary.to_str().unwrap()).unwrap();
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .unwrap();
        conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[])
            .unwrap();
        drop(conn);
        fs::copy(
            &primary,
            mcp_agent_mail_db::snapshot::snapshot_bak_path(&primary),
        )
        .unwrap();
        let original = mcp_agent_mail_db::snapshot::record_snapshot_metadata(&primary, 41).unwrap();
        for day in 1..=2 {
            fs::write(
                root.join(format!("storage.sqlite3.bak.2026010{day}_000000")),
                b"older evidence",
            )
            .unwrap();
        }
        let mut observations = Vec::new();
        let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "0")],
            || {
                rotate_storage_backups_with(&root, &primary, 1, |source, directory, metadata| {
                    // The native publication API runs at each real move. It must
                    // not replace the metadata that selected this pass's pin.
                    observations.push(
                        mcp_agent_mail_db::snapshot::record_snapshot_metadata(&primary, 42)
                            .is_err(),
                    );
                    stage_backup_noreplace(source, directory, metadata)
                })
                .unwrap()
            },
        );
        assert_eq!(report.staged, 2);
        assert_eq!(
            observations,
            [true, true],
            "metadata changed during retention's eviction interval"
        );
        assert_eq!(
            mcp_agent_mail_db::snapshot::read_snapshot_metadata(&primary),
            Some(original)
        );
        assert_eq!(
            mcp_agent_mail_db::snapshot::record_snapshot_metadata(&primary, 43)
                .unwrap()
                .created_us,
            43
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotation_coordination_refuses_live_snapshot_owners_before_eviction() {
        use fs2::FileExt as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        for shared in [false, true] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let primary = root.join("storage.sqlite3");
            let older = root.join("storage.sqlite3.bak.20260101_000000");
            let newer = root.join("storage.sqlite3.bak.20260102_000000");
            fs::write(&older, b"old backup evidence").unwrap();
            fs::write(&newer, b"new backup evidence").unwrap();
            let lock_path = root.join("storage.sqlite3.bak.authority.lock");
            let owner = fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&lock_path)
                .unwrap();
            let inode = owner.metadata().unwrap().ino();
            if shared {
                fs2::FileExt::lock_shared(&owner).unwrap();
            } else {
                owner.lock_exclusive().unwrap();
            }
            let report = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("AM_BACKUP_ROTATION_DELETE", "0")],
                || rotate_storage_backups(&root, &primary, 1),
            )
            .unwrap();
            drop(owner);
            assert!(
                !report.failures.is_empty(),
                "retention ignored a live snapshot owner: shared={shared}"
            );
            assert_eq!((report.deleted, report.staged, report.kept), (0, 0, 2));
            assert_eq!(report.bytes_retained(), 38);
            assert_eq!(fs::read(&older).unwrap(), b"old backup evidence");
            assert_eq!(fs::read(&newer).unwrap(), b"new backup evidence");
            assert!(
                !root.join("doctor").exists(),
                "busy admission created quarantine"
            );
            assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
            let resumed = rotate_with_delete_off(&root, 1);
            assert_eq!((resumed.staged, resumed.kept), (1, 1));
            assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
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

        assert_eq!(report.staged, 0);
        assert!(!tmp.path().join("doctor").exists());
        let oldest = tmp
            .path()
            .join("storage.sqlite3.corrupt-20260419_120000_000");
        assert_eq!(
            report.deleted, 0,
            "opt-in cannot replace verification authority"
        );
        assert_eq!(report.kept, 3);
        assert_eq!(report.bytes_deleted, 0);
        assert_eq!(report.failures.len(), 2);
        assert!(oldest.exists());
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
    fn incomplete_operations_never_claim_retained_or_reclaimed_bytes_twice() {
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
            deletion_completed: false,
            requested_destination: Some(PathBuf::from("quarantine/source")),
            error: "post-move validation failure".to_string(),
        };
        summary.record_failure(&completed, 11);
        let unconfirmed_unlink = RotationFailure {
            source: PathBuf::from("deleted-source"),
            rename_completed: false,
            deletion_completed: true,
            requested_destination: None,
            error: "unlink completed; directory sync failed".to_string(),
        };
        summary.record_failure(&unconfirmed_unlink, 13);
        assert_eq!(summary.kept, 1);
        assert_eq!(summary.bytes_kept, 7);
        assert_eq!(summary.staged, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.bytes_staged, 0);
        assert_eq!(summary.unconfirmed_moves, 1);
        assert_eq!(summary.bytes_unconfirmed_moves, 11);
        assert_eq!(summary.unconfirmed_deletions, 1);
        assert_eq!(summary.bytes_unconfirmed_deletions, 13);
        let mut report = RotateReport::default();
        report.add_kind(BackupKind::ManualBackup.label(), &summary);
        assert_eq!(report.evicted(), 0);
        assert_eq!(report.bytes_retained(), 18);
        assert_eq!(report.bytes_reclaimed, 0);
        assert_eq!(report.bytes_unconfirmed_deletions, 13);
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn empty_rotation_backlog_drains_past_the_inventory_limit_without_losing_evidence() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("storage.sqlite3");
        fs::write(&primary, b"live mailbox sentinel").unwrap();
        let reclaimable = tmp.path().join("doctor/reclaimable");
        fs::create_dir_all(&reclaimable).unwrap();
        let unknown = reclaimable.join("operator-empty-evidence");
        let recorded = reclaimable.join("rotation-20000101T000000Z");
        let nested = reclaimable.join("rotation-20000101T000000Z-1");
        fs::create_dir(&unknown).unwrap();
        fs::create_dir(&recorded).unwrap();
        fs::write(
            recorded.join("manifest.json"),
            b"recovery transaction evidence",
        )
        .unwrap();
        fs::create_dir_all(nested.join("unfinished-stage")).unwrap();

        // The first bounded sweep still leaves an incomplete inventory. A
        // cleanup that waits for a complete inventory can never make progress
        // on this real filesystem fixture, even though the directories are empty.
        let count = MAX_INVENTORY_ENTRIES + 2 * MAX_RECLAIM_DIRECTORIES_PER_PASS + 1;
        for index in 0..count {
            fs::create_dir(reclaimable.join(format!("rotation-20000101T000000Z-{}", index + 10)))
                .unwrap();
        }
        let remaining = || fs::read_dir(&reclaimable).unwrap().count() - 3;
        assert_eq!(remaining(), count);
        let error = admit_periodic_publication(tmp.path(), &primary, 3).unwrap_err();
        assert!(!periodic_publication_budget_exhausted(&error));

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "false")],
            || {
                let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
                assert!(report.inventory_incomplete);
                assert_eq!(
                    remaining(),
                    count,
                    "explicit false preserves empty directories"
                );
            },
        );
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || {
                let authority =
                    mcp_agent_mail_db::snapshot::SnapshotRetentionGuard::acquire(&primary).unwrap();
                let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
                assert!(report.admission_refused);
                assert_eq!(
                    remaining(),
                    count,
                    "busy snapshot authority excludes the empty sweep"
                );
                drop(authority);

                let mut previous = count;
                // Leave room for protected entries to consume inspection
                // work, while requiring a fixed bound and progress every pass.
                for pass in 0..=count.div_ceil(MAX_RECLAIM_DIRECTORIES_PER_PASS / 2) {
                    let report = rotate_storage_backups(tmp.path(), &primary, 3).unwrap();
                    let current = remaining();
                    assert!(current < previous, "pass {pass}: {report:?}");
                    assert!(
                        previous - current <= MAX_RECLAIM_DIRECTORIES_PER_PASS,
                        "pass {pass} exceeded its combined directory-removal budget"
                    );
                    if pass == 0 {
                        assert!(report.inventory_incomplete);
                        assert!(report.admission_refused);
                    }
                    assert_eq!(report.deleted, 0, "empty cleanup cannot claim file unlinks");
                    assert_eq!(report.bytes_reclaimed, 0);
                    assert_eq!(fs::read(&primary).unwrap(), b"live mailbox sentinel");
                    assert_eq!(
                        fs::read(recorded.join("manifest.json")).unwrap(),
                        b"recovery transaction evidence"
                    );
                    assert!(unknown.is_dir());
                    assert!(nested.join("unfinished-stage").is_dir());
                    if current == 0 {
                        admit_periodic_publication(tmp.path(), &primary, 3)
                            .expect("drained empty history restores normal publication admission");
                        return;
                    }
                    previous = current;
                }
                panic!("bounded empty-directory sweeps did not drain the legacy backlog");
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_incident_deletion_preserves_live_readers_and_unverified_evidence() {
        use std::os::unix::fs::MetadataExt as _;

        let tmp = TempDir::new().unwrap();
        let primary = verified_mailbox(tmp.path());
        let live = fs::read(&primary).unwrap();
        let paths: Vec<_> = (1..=5)
            .map(|day| {
                tmp.path()
                    .join(format!("storage.sqlite3.corrupt-2000010{day}_000000"))
            })
            .collect();
        fs::hard_link(&primary, &paths[0]).unwrap();
        fs::copy(&primary, &paths[1]).unwrap();
        fs::write(&paths[2], b"incomplete incident evidence").unwrap();
        fs::copy(&primary, &paths[3]).unwrap();
        fs::copy(&primary, &paths[4]).unwrap();
        for (index, path) in paths.iter().enumerate() {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new().set_modified(
                        SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
                    ),
                )
                .unwrap();
        }
        let reader = fs::File::open(&paths[1]).unwrap();
        let deletable = fs::metadata(&paths[3]).unwrap();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_BACKUP_ROTATION_DELETE", "true")],
            || {
                let report = rotate_storage_backups(tmp.path(), &primary, 1).unwrap();
                let incidents = &report.per_kind[BackupKind::Corrupt.label()];
                assert_eq!(incidents.deleted, 1, "{report:?}");
                assert_eq!(incidents.kept, 4);
                assert_eq!(report.bytes_deleted, deletable.len());
                assert_eq!(report.bytes_reclaimed, deletable.blocks() * 512);
                assert_eq!(report.failures.len(), 3);
                assert_eq!(fs::read(&paths[0]).unwrap(), live);
                assert_eq!(fs::read(&paths[1]).unwrap(), live);
                assert_eq!(
                    fs::read(&paths[2]).unwrap(),
                    b"incomplete incident evidence"
                );
                assert!(!paths[3].exists());
                assert_eq!(fs::read(&paths[4]).unwrap(), live);
                assert_eq!(fs::read(&primary).unwrap(), live);

                drop(reader);
                let resumed = rotate_storage_backups(tmp.path(), &primary, 1).unwrap();
                assert_eq!(resumed.per_kind[BackupKind::Corrupt.label()].deleted, 1);
                assert!(
                    !paths[1].exists(),
                    "reader release permits verified reclamation"
                );
                assert_eq!(fs::read(&paths[0]).unwrap(), live);
                assert_eq!(
                    fs::read(&paths[2]).unwrap(),
                    b"incomplete incident evidence"
                );
                assert_eq!(fs::read(&primary).unwrap(), live);
                assert!(
                    mcp_agent_mail_db::snapshot::verified_snapshot_source_path(&primary).is_some()
                );
            },
        );
    }
}
