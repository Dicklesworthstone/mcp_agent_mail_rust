//! Stable per-primary admission for snapshot metadata and verified selection.
//!
//! This file is never unlinked on release. Its inode is the cooperating
//! processes' authority; a replaced or aliased lock is an error, not stale
//! evidence to delete. Admission is nonblocking, including within one process.

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(any(unix, windows))]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
#[cfg(any(unix, windows))]
use std::path::{Component, PathBuf};
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use fs2::FileExt as _;
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

#[cfg(unix)]
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(unix)]
const MAX_RETENTION_SEARCH_CANDIDATES: usize = 128;
#[cfg(unix)]
const MAX_RETENTION_SEARCH_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(unix)]
const MAX_RETENTION_SEARCH_WITNESSES: usize = 16_384;

/// A maintenance slice ended before snapshot selection could finish. This is
/// not missing verification and must never authorize a repair publication.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRetentionDeferred {
    Deadline,
    Candidates,
    Bytes,
    InventoryChanged,
}

#[cfg(unix)]
impl std::fmt::Display for SnapshotRetentionDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "snapshot retention search deferred: {self:?}")
    }
}

#[cfg(unix)]
impl std::error::Error for SnapshotRetentionDeferred {}

#[cfg(unix)]
impl SnapshotRetentionDeferred {
    #[must_use]
    pub fn from_io_error(error: &io::Error) -> Option<Self> {
        error.get_ref()?.downcast_ref::<Self>().copied()
    }
}

#[cfg(unix)]
struct RetentionSearchBudget {
    deadline: Instant,
    candidates: usize,
    bytes: u64,
    candidate_limit: usize,
    byte_limit: u64,
}

#[cfg(unix)]
impl RetentionSearchBudget {
    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            candidates: 0,
            bytes: 0,
            candidate_limit: MAX_RETENTION_SEARCH_CANDIDATES,
            byte_limit: MAX_RETENTION_SEARCH_BYTES,
        }
    }

    fn admit(&mut self, bytes: u64) -> io::Result<()> {
        let refused = if Instant::now() >= self.deadline {
            Some(SnapshotRetentionDeferred::Deadline)
        } else if self.candidates >= self.candidate_limit {
            Some(SnapshotRetentionDeferred::Candidates)
        } else if self.candidates > 0 && self.bytes.saturating_add(bytes) > self.byte_limit {
            Some(SnapshotRetentionDeferred::Bytes)
        } else {
            None
        };
        if let Some(reason) = refused {
            return Err(io::Error::other(reason));
        }
        // One oversized source may finish, so a large real pin can eventually
        // be verified. A slice is a cooperative bound, not syscall preemption.
        self.candidates = self.candidates.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct RetentionSearchHint {
    metadata: Vec<u8>,
    path: PathBuf,
    selected: bool,
    witnesses: Vec<RetentionCandidateWitness>,
    remaining: Vec<PathBuf>,
    reader_refusal: Option<(io::ErrorKind, String)>,
}

/// A completed part of a search may be resumed only while the entire source
/// set is unchanged. These witnesses never grant positive verification or
/// deletion authority; completion without a pin can only reach the existing
/// one-shot, fully verified repair-publication admission.
#[cfg(unix)]
#[derive(Clone, PartialEq, Eq)]
struct RetentionCandidateWitness {
    path: PathBuf,
    identity: (u64, u64, u64, u64),
    owner_and_mode: (u32, u32, u32),
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
fn retention_candidate_witnesses(
    candidates: &[PathBuf],
) -> io::Result<Vec<RetentionCandidateWitness>> {
    if candidates.len() > MAX_RETENTION_SEARCH_WITNESSES {
        return Err(io::Error::other(
            "retention search inventory exceeds its entry bound",
        ));
    }
    let mut witnesses = Vec::with_capacity(candidates.len());
    for path in candidates {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(invalid("retention search candidate changed its file type"));
        }
        witnesses.push(RetentionCandidateWitness {
            path: path.clone(),
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.nlink(),
                metadata.len(),
            ),
            owner_and_mode: (metadata.uid(), metadata.gid(), metadata.mode()),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        });
    }
    witnesses.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(witnesses)
}

#[cfg(unix)]
fn retention_search_hints()
-> &'static std::sync::Mutex<std::collections::BTreeMap<PathBuf, RetentionSearchHint>> {
    static HINTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<PathBuf, RetentionSearchHint>>,
    > = std::sync::OnceLock::new();
    HINTS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

#[cfg(unix)]
pub(super) struct SnapshotAuthority {
    file: File,
    parent: File,
    parent_path: PathBuf,
    name: OsString,
}

/// Exclusive snapshot admission retained from pin selection through eviction.
///
/// This coordinates retention with metadata publishers, verified readers, and
/// the durable recovery election. Holding the election excludes candidate
/// selection/staging without recording a recovery attempt or opening the live
/// database. The caller must still validate each inventoried file before moving it.
#[cfg(unix)]
#[must_use]
pub struct SnapshotRetentionGuard {
    authority: SnapshotAuthority,
    expected_parent: Option<File>,
    primary: PathBuf,
    metadata_path: PathBuf,
    metadata: RetentionMetadata,
    selected: Option<super::SelectedVerifiedSnapshot>,
    selected_identity: Option<std::fs::Metadata>,
    completed_search_without_pin: bool,
    // Drop recovery last, after releasing the snapshot authority and source.
    recovery: crate::recovery_breaker::RecoveryBreakerFileLock,
}

#[cfg(unix)]
#[derive(PartialEq, Eq)]
enum RetentionMetadata {
    Absent,
    Bytes(Vec<u8>),
    Unavailable,
}

#[cfg(unix)]
fn retention_metadata(path: &Path) -> RetentionMetadata {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => RetentionMetadata::Absent,
        Ok(metadata) if metadata.is_file() => {
            match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
                path,
                super::MAX_SNAPSHOT_METADATA_BYTES,
            ) {
                Ok(bytes) => RetentionMetadata::Bytes(bytes),
                Err(_) => RetentionMetadata::Unavailable,
            }
        }
        _ => RetentionMetadata::Unavailable,
    }
}

#[cfg(unix)]
impl SnapshotRetentionGuard {
    /// Acquire without waiting, then select the exact verified generation.
    /// An unreadable or malformed sidecar stays occupied, never absent.
    ///
    /// # Errors
    /// Refuses recovery, live readers/publishers, pending recovery intents,
    /// unsafe lock paths, or changed authority.
    pub fn acquire(primary: &Path) -> io::Result<Self> {
        Self::acquire_with_parent(primary, None, None)
    }

    /// Bind retention to the directory already held by an exporter admission.
    /// The retained directory identity, rather than its reusable pathname,
    /// determines which snapshot authority may authorize cleanup.
    ///
    /// # Errors
    /// Refuses an already replaced parent before acquiring control files, a captured
    /// snapshot authority in another directory, and every refusal of
    /// [`Self::acquire`]. Concurrent namespace changes remain a refusal.
    pub fn acquire_for_parent(primary: &Path, expected_parent: &File) -> io::Result<Self> {
        Self::acquire_with_parent(primary, Some(expected_parent), None)
    }

    /// Admit bounded background work without treating an unfinished pin search
    /// as an absent pin. Process-local progress can resume an unchanged source
    /// set, but every selected pin rechecks the recorded bytes and full
    /// integrity. A completed search without a pin grants no deletion rights.
    /// A source already being checked may finish after the cooperative deadline.
    ///
    /// # Errors
    /// Returns [`SnapshotRetentionDeferred`] inside the I/O error when work is
    /// deferred, and preserves every refusal of [`Self::acquire_for_parent`].
    pub fn acquire_for_parent_until(
        primary: &Path,
        expected_parent: &File,
        deadline: Instant,
    ) -> io::Result<Self> {
        Self::acquire_with_parent(
            primary,
            Some(expected_parent),
            Some(RetentionSearchBudget::new(deadline)),
        )
    }

    fn acquire_with_parent(
        primary: &Path,
        expected_parent: Option<&File>,
        mut budget: Option<RetentionSearchBudget>,
    ) -> io::Result<Self> {
        let primary = super::normalized_snapshot_primary(primary).map_err(io::Error::other)?;
        let expected_parent = expected_parent.map(File::try_clone).transpose()?;
        // Recovery owns this election before selecting a snapshot. Use the
        // same ordering and one retained parent for both controls, including
        // standalone retention without a borrowed exporter admission.
        let (recovery, authority) =
            acquire_retention_controls_with(&primary, expected_parent.as_ref(), || {}, || {})?;
        ensure_no_recovery_references(&primary)?;
        let metadata_path = super::snapshot_meta_path(&primary);
        let metadata = retention_metadata(&metadata_path);
        let reader_refusal = std::cell::RefCell::<Option<io::Error>>::new(None);
        let bounded = budget.is_some();
        let mut completed_search_without_pin = false;
        let selected = if let RetentionMetadata::Bytes(metadata_bytes) = &metadata {
            let hint = budget.as_ref().and_then(|_| {
                retention_search_hints()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&primary)
                    .filter(|hint| hint.metadata == *metadata_bytes)
                    .cloned()
            });
            let mut last_attempted = None;
            let mut witnesses = Vec::new();
            let mut ordered_candidates = Vec::new();
            let mut inventoried = false;
            // Already exclusive: reacquiring the public selector's shared lock
            // would reject our own admission and discard a valid pin.
            let mut selected = super::select_verified_snapshot_with_work_admission(
                &primary,
                |candidates| {
                    if bounded {
                        witnesses = retention_candidate_witnesses(candidates)?;
                        inventoried = true;
                        if let Some(hint) = hint {
                            if hint.selected {
                                if let Some(index) =
                                    candidates.iter().position(|path| path == &hint.path)
                                {
                                    let path = candidates.remove(index);
                                    candidates.insert(0, path);
                                }
                            } else if hint.witnesses == witnesses {
                                // Continue only the unfinished suffix of this
                                // exact source set. Rotating the whole set
                                // would never complete an all-invalid backlog.
                                *candidates = hint.remaining;
                                *reader_refusal.borrow_mut() = hint
                                    .reader_refusal
                                    .map(|(kind, message)| io::Error::new(kind, message));
                            }
                        }
                        ordered_candidates.clone_from(candidates);
                    }
                    Ok(())
                },
                |candidate, inventoried| {
                    #[cfg(target_os = "linux")]
                    {
                        match crate::recovery_retention::preflight_current_thread_file_locks(
                            candidate,
                            inventoried,
                        ) {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                let refusal = io::Error::new(error.kind(), error.to_string());
                                reader_refusal.borrow_mut().get_or_insert(error);
                                Err(refusal)
                            }
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = (candidate, inventoried);
                        Ok(())
                    }
                },
                |candidate, inventoried| {
                    if let Some(budget) = budget.as_mut() {
                        // A matching pin requires a hash and full integrity
                        // scan. Charge both before admitting the candidate.
                        budget.admit(inventoried.len().saturating_mul(2))?;
                        last_attempted = Some(candidate.to_path_buf());
                    }
                    Ok(())
                },
            );
            if bounded && inventoried && matches!(selected, Ok(None)) {
                // A completed search is not an authority to delete anything.
                // It may reach the separately bounded repair-export path,
                // but only after rechecking every source witness: a new or
                // changed earlier candidate invalidates the entire sweep.
                let current = crate::pool::sqlite_backup_candidates(&primary);
                if retention_candidate_witnesses(&current)? == witnesses {
                    completed_search_without_pin = true;
                } else {
                    selected = Err(io::Error::other(
                        SnapshotRetentionDeferred::InventoryChanged,
                    ));
                }
            }
            if bounded {
                let next = match &selected {
                    Ok(Some(selected)) => Some(RetentionSearchHint {
                        metadata: metadata_bytes.clone(),
                        path: selected.actual_path.clone(),
                        selected: true,
                        witnesses: Vec::new(),
                        remaining: Vec::new(),
                        reader_refusal: None,
                    }),
                    Ok(None)
                        if completed_search_without_pin && reader_refusal.borrow().is_none() =>
                    {
                        // Keep completion available to a later producer even
                        // when a cleanup-only continuation finished the sweep.
                        // Its whole source set is re-witnessed on every reuse,
                        // and it cannot supply a pin to any deletion method.
                        Some(RetentionSearchHint {
                            metadata: metadata_bytes.clone(),
                            path: primary.clone(),
                            selected: false,
                            witnesses,
                            remaining: Vec::new(),
                            reader_refusal: None,
                        })
                    }
                    Err(error)
                        if matches!(
                            SnapshotRetentionDeferred::from_io_error(error),
                            Some(
                                SnapshotRetentionDeferred::Deadline
                                    | SnapshotRetentionDeferred::Candidates
                                    | SnapshotRetentionDeferred::Bytes
                            )
                        ) && inventoried =>
                    {
                        let start = last_attempted
                            .as_ref()
                            .and_then(|last| {
                                ordered_candidates.iter().position(|path| path == last)
                            })
                            .map_or(0, |index| index + 1);
                        ordered_candidates
                            .get(start)
                            .map(|path| RetentionSearchHint {
                                metadata: metadata_bytes.clone(),
                                path: path.clone(),
                                selected: false,
                                witnesses,
                                remaining: ordered_candidates[start..].to_vec(),
                                reader_refusal: reader_refusal
                                    .borrow()
                                    .as_ref()
                                    .map(|error| (error.kind(), error.to_string())),
                            })
                    }
                    _ => None,
                };
                let mut hints = retention_search_hints()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Bound process-local progress across mailboxes as well as
                // within one mailbox. Eviction only restarts work; it never
                // bypasses a check or creates persistent authority.
                hints.remove(&primary);
                if let Some(next) = next {
                    while hints.len() >= 128
                        || hints
                            .values()
                            .map(|hint| hint.witnesses.len())
                            .sum::<usize>()
                            .saturating_add(next.witnesses.len())
                            > MAX_RETENTION_SEARCH_WITNESSES
                    {
                        hints.pop_first();
                    }
                    hints.insert(primary.clone(), next);
                }
            }
            selected?
        } else {
            None
        };
        // A verified fallback may bypass a busy earlier candidate. Without
        // that fallback, preserve the admission refusal: treating a blocked
        // pin as missing verification could authorize a repair export.
        #[cfg(target_os = "linux")]
        if selected.is_none()
            && let Some(error) = reader_refusal.into_inner()
        {
            return Err(error);
        }
        let selected_identity = selected
            .as_ref()
            .map(|selected| selected.source.metadata())
            .transpose()?;
        let guard = Self {
            authority,
            expected_parent,
            primary,
            metadata_path,
            metadata,
            selected,
            selected_identity,
            completed_search_without_pin,
            recovery,
        };
        guard.revalidate()?;
        Ok(guard)
    }

    /// Whether a sidecar exists or its absence could not be established.
    #[must_use]
    pub fn metadata_occupied(&self) -> bool {
        self.metadata != RetentionMetadata::Absent
    }

    /// The verified backup that must remain outside quarantine this pass.
    #[must_use]
    pub fn verified_source_path(&self) -> Option<&Path> {
        self.selected
            .as_ref()
            .map(|selected| selected.actual_path.as_path())
    }

    /// Verified pin length for charging repeated verification work, never a
    /// substitute for revalidating its identity or contents before an unlink.
    #[must_use]
    pub fn verified_source_size_bytes(&self) -> Option<u64> {
        self.selected
            .as_ref()
            .map(|selected| selected.metadata.snapshot_size_bytes)
    }

    /// A complete bounded sweep found no verified pin in its unchanged source
    /// set. This is only a completion signal for repair publication; all
    /// deletion methods still require a freshly verified positive pin.
    #[must_use]
    pub const fn completed_search_without_pin(&self) -> bool {
        self.completed_search_without_pin
    }

    /// Reclaim a strict, dated periodic generation only when a newer, fully
    /// verified snapshot remains available under this same exclusive admission.
    /// The caller selects the retention window; this boundary enforces safety.
    ///
    /// On Linux, a persistent denial of another process's procfs inventory is
    /// recorded as uncertainty, not proof of reader absence. An owned snapshot
    /// can still be unlinked after all checks below: existing descriptors and
    /// mappings retain the old inode's bytes, while its pathname disappears.
    /// Known readers and our own process's unobservable readers still refuse.
    ///
    /// # Errors
    /// Refuses unresolved verification, unproven database lineage, a pinned or
    /// changed file, companion recovery state, newer generations, and unsafe
    /// namespace operations. Unknown lineage requires exact duplicate bytes.
    pub fn delete_superseded_generation(
        &self,
        primary: &Path,
        path: &Path,
        inventoried: &std::fs::Metadata,
    ) -> io::Result<crate::recovery_retention::ReclaimedFileBytes> {
        self.delete_superseded_generation_with(primary, path, inventoried, false)
    }

    /// Historical quarantine names do not prove which database produced them.
    /// Require a shared persistent DB generation token, or a byte-identical
    /// duplicate of the pinned snapshot, before applying periodic retention.
    ///
    /// # Errors
    /// Refuses unknown or conflicting lineage unless the bytes are identical,
    /// in addition to every refusal of [`Self::delete_superseded_generation`].
    pub fn delete_superseded_historical_generation(
        &self,
        primary: &Path,
        path: &Path,
        inventoried: &std::fs::Metadata,
    ) -> io::Result<crate::recovery_retention::ReclaimedFileBytes> {
        self.delete_superseded_generation_with(primary, path, inventoried, false)
    }

    /// Apply an explicit operator deletion policy to a superseded incident
    /// image. Opt-in changes eligible naming, never recovery verification or
    /// ownership safety. Unverified incident evidence stays available.
    ///
    /// # Errors
    /// Refuses live/pinned images, incomplete SQLite families, readers, aliases,
    /// changed inventory, and incident images without a verified successor.
    /// Archive-reconcile evidence remains owned by its separate recovery path.
    pub fn delete_superseded_incident_generation(
        &self,
        primary: &Path,
        path: &Path,
        inventoried: &std::fs::Metadata,
    ) -> io::Result<crate::recovery_retention::ReclaimedFileBytes> {
        self.delete_superseded_generation_with(primary, path, inventoried, true)
    }

    fn delete_superseded_generation_with(
        &self,
        primary: &Path,
        path: &Path,
        inventoried: &std::fs::Metadata,
        incident_opted_in: bool,
    ) -> io::Result<crate::recovery_retention::ReclaimedFileBytes> {
        self.revalidate()?;
        let primary = super::normalized_snapshot_primary(primary).map_err(io::Error::other)?;
        if super::snapshot_meta_path(&primary) != self.metadata_path {
            return Err(invalid("retention authority belongs to another database"));
        }
        let selected = self
            .selected
            .as_ref()
            .ok_or_else(|| invalid("no verified replacement"))?;
        let created = retention_generation_created(&primary, path, inventoried, incident_opted_in)?;
        if created
            .is_none_or(|created| created.saturating_add(1_000_000) > selected.metadata.created_us)
        {
            return Err(invalid("generation is not superseded by verified snapshot"));
        }
        let pinned = selected.source.metadata()?;
        if pinned.dev() == inventoried.dev() && pinned.ino() == inventoried.ino() {
            return Err(invalid("cannot reclaim the verified snapshot"));
        }
        if inventoried.nlink() != 1 || std::fs::symlink_metadata(path)?.nlink() != 1 {
            return Err(invalid("cannot reclaim an aliased backup generation"));
        }
        if !crate::pool::sqlite_recovery_candidate_is_standalone(path) {
            return Err(invalid("generation has companion recovery state"));
        }
        let reader_evidence = if incident_opted_in {
            // Incident evidence retains the strict external-reader policy.
            crate::recovery_retention::preflight_inventoried_file_readers(path, inventoried)?;
            None
        } else {
            Some(
                crate::recovery_retention::preflight_superseded_snapshot_readers(
                    path,
                    inventoried,
                )?,
            )
        };
        // A dated spelling is not a completed, recoverable publication.
        // Truncated files, unrelated SQLite schemas, and corrupt snapshots
        // remain evidence even when a newer healthy restore point exists.
        match crate::pool::sqlite_recovery_candidate_passes_full_integrity_check(path) {
            Ok(true) => {}
            Ok(false) => return Err(invalid("backup generation failed full verification")),
            Err(error) => {
                return Err(io::Error::other(format!(
                    "backup generation verification unavailable: {error}"
                )));
            }
        }
        // Neither residence beside the primary nor a quarantine timestamp
        // proves provenance: an earlier physical DB reconstruction or another
        // mailbox may have left a perfectly valid but distinct recovery image.
        let same_generation = immutable_generation_id(path)
            .zip(immutable_generation_id(&selected.actual_path))
            .is_some_and(|(candidate, replacement)| candidate == replacement);
        if !same_generation {
            let identity = super::snapshot_file_identity(path).map_err(io::Error::other)?;
            if identity.0 != selected.metadata.snapshot_size_bytes
                || identity.1 != selected.metadata.snapshot_sha256
            {
                return Err(invalid(
                    "backup has no proven matching database lineage or duplicate bytes",
                ));
            }
        }
        // Admission already performed full integrity verification. Rehash the
        // exact pinned inode rather than rescanning every generation for every
        // eviction (quadratic IO on an accumulated multi-week backlog).
        let mut current =
            mcp_agent_mail_core::disk::open_regular_file_no_follow(&selected.actual_path)?;
        let current_metadata = current.metadata()?;
        if current_metadata.dev() != pinned.dev()
            || current_metadata.ino() != pinned.ino()
            || !crate::pool::sqlite_recovery_candidate_is_standalone(&selected.actual_path)
        {
            return Err(invalid("verified replacement changed during retention"));
        }
        let identity =
            super::snapshot_file_identity_from_reader(&mut current, &selected.actual_path)
                .map_err(io::Error::other)?;
        if identity.0 != selected.metadata.snapshot_size_bytes
            || identity.1 != selected.metadata.snapshot_sha256
        {
            return Err(invalid(
                "verified replacement bytes changed during retention",
            ));
        }
        self.revalidate()?;
        match reader_evidence {
            Some(readers) => crate::recovery_retention::delete_superseded_snapshot_file(
                path,
                inventoried,
                readers,
            ),
            None => crate::recovery_retention::delete_inventoried_file(path, inventoried),
        }
    }

    /// Recheck ownership and metadata before another destructive operation.
    /// Participating writers are excluded; changed external evidence refuses
    /// further work rather than allowing a stale pin to authorize eviction.
    pub fn revalidate(&self) -> io::Result<()> {
        if let Some(expected) = &self.expected_parent {
            self.authority.ensure_parent(expected)?;
        }
        self.recovery.revalidate()?;
        self.authority.revalidate()?;
        ensure_no_recovery_references(&self.primary)?;
        if retention_metadata(&self.metadata_path) != self.metadata {
            return Err(invalid(
                "snapshot metadata changed during retention; rescan required",
            ));
        }
        if let (Some(selected), Some(expected)) = (&self.selected, &self.selected_identity) {
            let retained = selected.source.metadata()?;
            let current = std::fs::symlink_metadata(&selected.actual_path)?;
            for actual in [&retained, &current] {
                if !actual.is_file()
                    || actual.nlink() != 1
                    || actual.dev() != expected.dev()
                    || actual.ino() != expected.ino()
                    || actual.len() != expected.len()
                    || actual.mtime() != expected.mtime()
                    || actual.mtime_nsec() != expected.mtime_nsec()
                    || actual.ctime() != expected.ctime()
                    || actual.ctime_nsec() != expected.ctime_nsec()
                {
                    return Err(invalid("verified snapshot pin changed during retention"));
                }
            }
            if !crate::pool::sqlite_recovery_candidate_is_standalone(&selected.actual_path) {
                return Err(invalid(
                    "verified snapshot pin gained companion recovery state",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn retention_generation_created(
    primary: &Path,
    path: &Path,
    inventoried: &std::fs::Metadata,
    incident_opted_in: bool,
) -> io::Result<Option<i64>> {
    use mcp_agent_mail_core::disk::{
        SqliteRecoveryCandidateKind, SqliteRecoveryCandidateName,
        classify_sqlite_recovery_candidate_name,
    };

    let candidate = primary
        .file_name()
        .zip(path.file_name())
        .and_then(|(db, name)| classify_sqlite_recovery_candidate_name(db, name));
    if candidate
        .is_some_and(|candidate| candidate.kind() == SqliteRecoveryCandidateKind::TimestampedBak)
    {
        return Ok(candidate.and_then(SqliteRecoveryCandidateName::generation_micros));
    }
    if incident_opted_in
        && super::normalized_snapshot_primary(path)
            .map_err(io::Error::other)?
            .parent()
            == primary.parent()
        && path.file_name() != super::snapshot_bak_path(primary).file_name()
        && primary
            .file_name()
            .zip(path.file_name())
            .is_some_and(|(db, name)| {
                crate::recovery_retention::classify_backup_file(db, name).is_some_and(|kind| {
                    kind != crate::recovery_retention::BackupKind::ArchiveReconcile
                })
            })
    {
        // Incident names have several historical grammars. Treat a
        // copied/unknown mtime conservatively and still demand full
        // verification plus proven lineage before any unlink.
        return Ok(inventoried
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|age| i64::try_from(age.as_micros()).ok()));
    }
    Err(invalid("not an eligible superseded backup generation"))
}

#[cfg(unix)]
fn ensure_no_recovery_references(primary: &Path) -> io::Result<()> {
    let legacy_lock = crate::pool::sqlite_path_with_suffix(primary, ".recovery.lock");
    match std::fs::symlink_metadata(&legacy_lock) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(_) => {
            // Older processes do not participate in the durable election.
            // Invalid or stale-looking PID text cannot authorize evidence loss.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "legacy recovery lock remains occupied; preserving recovery evidence",
            ));
        }
    }
    crate::forensics::ensure_no_pending_recovery_receipts(primary)
}

/// Read only through immutable canonical SQLite; never seed or repair a
/// candidate's identity to make it eligible for deletion. Unknown legacy
/// schemas, malformed identities, and query errors require byte equality.
#[cfg(unix)]
fn immutable_generation_id(path: &Path) -> Option<String> {
    let connection = crate::pool::open_immutable_canonical_sqlite(path).ok()?;
    let rows = connection.query_sync(
        "SELECT singleton, substr(generation_id, 1, 257) AS generation_id FROM db_identity LIMIT 2",
        &[],
    ).ok()?;
    if rows.len() != 1 || rows[0].get_named::<i64>("singleton").ok()? != 0 {
        return None;
    }
    let generation = rows[0].get_named::<String>("generation_id").ok()?;
    (!generation.is_empty() && generation.len() <= 256).then_some(generation)
}

#[cfg(any(unix, windows))]
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(unix)]
fn ensure_same_directory(captured: &File, expected: &File) -> io::Result<()> {
    let captured = captured.metadata()?;
    let expected = expected.metadata()?;
    if !captured.is_dir()
        || !expected.is_dir()
        || captured.dev() != expected.dev()
        || captured.ino() != expected.ino()
    {
        return Err(invalid(
            "snapshot retention parent differs from exporter admission",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_retention_controls_with(
    primary: &Path,
    expected_parent: Option<&File>,
    after_parent_capture: impl FnOnce(),
    after_recovery: impl FnOnce(),
) -> io::Result<(
    crate::recovery_breaker::RecoveryBreakerFileLock,
    SnapshotAuthority,
)> {
    let parent_path = primary
        .parent()
        .ok_or_else(|| invalid("snapshot primary has no parent"))?;
    let parent = open_directory(parent_path)?;
    if let Some(expected) = expected_parent {
        ensure_same_directory(&parent, expected)?;
    }
    after_parent_capture();
    // No outcome/backoff state is read or changed by this election. Both
    // constructors validate the pathname, then operate only on this inode.
    let recovery = crate::recovery_breaker::try_acquire_file_lock_in_parent(primary, &parent)?;
    after_recovery();
    let authority = SnapshotAuthority::acquire_in_parent(primary, true, &parent)?;
    recovery.revalidate()?;
    authority.revalidate()?;
    Ok((recovery, authority))
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid("snapshot authority requires an absolute parent"));
    }
    let mut directory = File::from(rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty())?);
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = File::from(rustix::fs::openat(
                    &directory,
                    name,
                    DIRECTORY_FLAGS,
                    Mode::empty(),
                )?);
            }
            _ => return Err(invalid("snapshot authority refuses parent traversal")),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
impl SnapshotAuthority {
    fn ensure_parent(&self, expected: &File) -> io::Result<()> {
        ensure_same_directory(&self.parent, expected)
    }

    pub(super) fn acquire(primary: &Path, exclusive: bool) -> io::Result<Self> {
        let parent_path = primary
            .parent()
            .ok_or_else(|| invalid("snapshot primary has no parent"))?;
        let parent = open_directory(parent_path)?;
        Self::acquire_in_parent(primary, exclusive, &parent)
    }

    fn acquire_in_parent(
        primary: &Path,
        exclusive: bool,
        expected_parent: &File,
    ) -> io::Result<Self> {
        Self::acquire_in_parent_with(primary, exclusive, expected_parent, || {}, || {})
    }

    fn acquire_in_parent_with(
        primary: &Path,
        exclusive: bool,
        expected_parent: &File,
        after_parent_open: impl FnOnce(),
        after_parent_bound: impl FnOnce(),
    ) -> io::Result<Self> {
        let parent_path = primary
            .parent()
            .ok_or_else(|| invalid("snapshot primary has no parent"))?
            .to_path_buf();
        let observed_parent = open_directory(&parent_path)?;
        after_parent_open();
        ensure_same_directory(&observed_parent, expected_parent)?;
        // Capture the caller's directory authority before any control-file
        // operation. A later pathname replacement cannot redirect creation.
        let parent = expected_parent.try_clone()?;
        after_parent_bound();
        let mut name = primary
            .file_name()
            .ok_or_else(|| invalid("snapshot primary has no leaf"))?
            .to_os_string();
        name.push(".bak.authority.lock");
        // Readers of an existing snapshot need no write permission. A legacy
        // snapshot first acquires its persistent lock via create-only admission.
        let access = if exclusive {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let read_flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let (file, created) = match rustix::fs::openat(&parent, &name, read_flags, Mode::empty()) {
            Ok(file) => (File::from(file), false),
            Err(error) if error == rustix::io::Errno::NOENT => {
                match rustix::fs::openat(
                    &parent,
                    &name,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                ) {
                    Ok(file) => (File::from(file), true),
                    Err(error) if error == rustix::io::Errno::EXIST => (
                        File::from(rustix::fs::openat(
                            &parent,
                            &name,
                            read_flags,
                            Mode::empty(),
                        )?),
                        false,
                    ),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let guard = Self {
            file,
            parent,
            parent_path,
            name,
        };
        guard.revalidate()?;
        if exclusive {
            guard.file.try_lock_exclusive()?;
        } else {
            guard.file.try_lock_shared()?;
        }
        guard.revalidate()?;
        if created {
            guard.file.sync_all()?;
            guard.parent.sync_all()?;
        }
        Ok(guard)
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        let retained = self.file.metadata()?;
        if !retained.is_file()
            || retained.nlink() != 1
            || retained.len() != 0
            || retained.mode() & 0o777 != 0o600
        {
            return Err(invalid(
                "snapshot authority must be an empty private single-link regular file",
            ));
        }
        // Compare the entry with the retained file in the same `Stat` types:
        // `st_dev` is i32 on macOS but u64 on Linux, while std's `Metadata`
        // always widens to u64, so mixing the two does not compile on macOS.
        let entry = rustix::fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)?;
        let retained_stat = rustix::fs::fstat(&self.file)?;
        if FileType::from_raw_mode(entry.st_mode) != FileType::RegularFile
            || entry.st_dev != retained_stat.st_dev
            || entry.st_ino != retained_stat.st_ino
        {
            return Err(invalid("snapshot authority lock identity changed"));
        }
        let current_parent = open_directory(&self.parent_path)?.metadata()?;
        let retained_parent = self.parent.metadata()?;
        if current_parent.dev() != retained_parent.dev()
            || current_parent.ino() != retained_parent.ino()
        {
            return Err(invalid("snapshot authority parent identity changed"));
        }
        Ok(())
    }
}

#[cfg(windows)]
const WINDOWS_SHARE_READ_WRITE: u32 = 0x0000_0001 | 0x0000_0002;
#[cfg(windows)]
const WINDOWS_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const WINDOWS_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const WINDOWS_REPARSE_POINT: u32 = 0x0000_0400;

/// Windows retains handles that deny delete sharing for the lock and every
/// ancestor. Path-based opens therefore cannot move to a replacement parent
/// while a publication or selected source still holds admission.
#[cfg(windows)]
pub(super) struct SnapshotAuthority {
    file: File,
    path: PathBuf,
    parents: Vec<(PathBuf, File)>,
}

#[cfg(windows)]
fn validate_windows_directory(directory: &File) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & WINDOWS_REPARSE_POINT != 0 {
        return Err(invalid(
            "snapshot authority parent must be a directory without reparse points",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(WINDOWS_SHARE_READ_WRITE)
        .custom_flags(WINDOWS_OPEN_REPARSE_POINT | WINDOWS_BACKUP_SEMANTICS)
        .open(path)?;
    validate_windows_directory(&directory)?;
    Ok(directory)
}

#[cfg(windows)]
fn pin_windows_parents(path: &Path) -> io::Result<Vec<(PathBuf, File)>> {
    if !path.is_absolute() {
        return Err(invalid("snapshot authority requires an absolute parent"));
    }
    let mut current = PathBuf::new();
    let mut parents = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir | Component::Normal(_) => current.push(component.as_os_str()),
            _ => return Err(invalid("snapshot authority refuses parent traversal")),
        }
        // Pin each ancestor before opening its child. Denying delete sharing
        // only on the immediate parent would still permit ancestor replacement.
        let directory = open_windows_directory(&current)?;
        parents.push((current.clone(), directory));
    }
    if parents.is_empty() {
        return Err(invalid("snapshot authority has no directory authority"));
    }
    Ok(parents)
}

#[cfg(windows)]
fn open_windows_lock(path: &Path, exclusive: bool, create: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .write(exclusive || create)
        .create_new(create)
        // Rust's default includes FILE_SHARE_DELETE, which would allow two
        // processes to lock different files at the same reusable pathname.
        .share_mode(WINDOWS_SHARE_READ_WRITE)
        .custom_flags(WINDOWS_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
impl SnapshotAuthority {
    pub(super) fn acquire(primary: &Path, exclusive: bool) -> io::Result<Self> {
        let parent = primary
            .parent()
            .ok_or_else(|| invalid("snapshot primary has no parent"))?;
        let parents = pin_windows_parents(parent)?;
        let mut name = primary
            .file_name()
            .ok_or_else(|| invalid("snapshot primary has no leaf"))?
            .to_os_string();
        name.push(".bak.authority.lock");
        let path = parent.join(name);
        let (file, created) = match open_windows_lock(&path, exclusive, false) {
            Ok(file) => (file, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match open_windows_lock(&path, exclusive, true) {
                    Ok(file) => (file, true),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        (open_windows_lock(&path, exclusive, false)?, false)
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let guard = Self {
            file,
            path,
            parents,
        };
        guard.revalidate()?;
        if exclusive {
            fs2::FileExt::try_lock_exclusive(&guard.file)?;
        } else {
            fs2::FileExt::try_lock_shared(&guard.file)?;
        }
        guard.revalidate()?;
        if created {
            // The new file inherits its directory's ACL, matching the core
            // private-file helpers. Existing sentinels are never rewritten.
            guard.file.sync_all()?;
        }
        Ok(guard)
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        use mcp_agent_mail_core::disk::{windows_file_identity, windows_file_link_count};
        use std::os::windows::fs::MetadataExt as _;

        for (path, retained) in &self.parents {
            validate_windows_directory(retained)?;
            let current = open_windows_directory(path)?;
            if windows_file_identity(retained)? != windows_file_identity(&current)? {
                return Err(invalid("snapshot authority parent identity changed"));
            }
        }
        let retained = self.file.metadata()?;
        if !retained.is_file()
            || retained.file_attributes() & WINDOWS_REPARSE_POINT != 0
            || retained.len() != 0
            || windows_file_link_count(&self.file)? != 1
        {
            return Err(invalid(
                "snapshot authority must be an empty single-link regular file without reparse points",
            ));
        }
        let current = mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.path)?;
        if windows_file_identity(&self.file)? != windows_file_identity(&current)? {
            return Err(invalid("snapshot authority lock identity changed"));
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) struct SnapshotAuthority;

#[cfg(not(any(unix, windows)))]
impl SnapshotAuthority {
    pub(super) fn acquire(_primary: &Path, _exclusive: bool) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "snapshot authority is unavailable on this platform",
        ))
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "snapshot authority is unavailable on this platform",
        ))
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use mcp_agent_mail_core::disk::{windows_file_identity, windows_file_link_count};

    #[test]
    fn shared_readers_exclude_publishers_without_serializing_other_primaries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let primary = root.join("mail.sqlite3");
        let first = SnapshotAuthority::acquire(&primary, false).unwrap();
        let identity = windows_file_identity(&first.file).unwrap();
        let lock_path = first.path.clone();
        let second = SnapshotAuthority::acquire(&primary, false).unwrap();
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        let other = SnapshotAuthority::acquire(&root.join("other.sqlite3"), true).unwrap();
        drop(first);
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        drop(second);
        let publisher = SnapshotAuthority::acquire(&primary, true).unwrap();
        assert_eq!(windows_file_identity(&publisher.file).unwrap(), identity);
        assert_eq!(windows_file_link_count(&publisher.file).unwrap(), 1);
        assert!(SnapshotAuthority::acquire(&primary, false).is_err());
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        drop(publisher);
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"");
        let reader = SnapshotAuthority::acquire(&primary, false).unwrap();
        assert_eq!(windows_file_identity(&reader.file).unwrap(), identity);
        drop(reader);
        drop(other);
    }

    #[test]
    fn admission_pins_the_lock_parent_and_ancestors_until_release() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let ancestor = root.join("workspace");
        let parent = ancestor.join("mailbox");
        std::fs::create_dir_all(&parent).unwrap();
        let primary = parent.join("mail.sqlite3");
        let guard = SnapshotAuthority::acquire(&primary, true).unwrap();
        assert!(std::fs::rename(&guard.path, parent.join("replaced-lock")).is_err());
        assert!(std::fs::rename(&parent, ancestor.join("replaced-mailbox")).is_err());
        assert!(std::fs::rename(&ancestor, root.join("replaced-workspace")).is_err());
        guard.revalidate().unwrap();
        let lock_name = guard.path.file_name().unwrap().to_os_string();
        drop(guard);
        let moved = ancestor.join("moved-mailbox");
        std::fs::rename(&parent, &moved).unwrap();
        assert_eq!(std::fs::read(moved.join(lock_name)).unwrap(), b"");
    }

    #[test]
    fn malformed_lock_entries_are_preserved_and_never_admitted() {
        for shape in ["contents", "hardlink", "directory"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let lock_path = root.join("mail.sqlite3.bak.authority.lock");
            let sentinel = root.join("sentinel");
            match shape {
                "contents" => std::fs::write(&lock_path, b"retained evidence").unwrap(),
                "hardlink" => {
                    std::fs::write(&sentinel, b"").unwrap();
                    std::fs::hard_link(&sentinel, &lock_path).unwrap();
                }
                _ => {
                    std::fs::create_dir(&lock_path).unwrap();
                    std::fs::write(lock_path.join("evidence"), b"retained evidence").unwrap();
                }
            }
            assert!(
                SnapshotAuthority::acquire(&primary, true).is_err(),
                "{shape}"
            );
            assert!(
                SnapshotAuthority::acquire(&primary, false).is_err(),
                "{shape}"
            );
            match shape {
                "contents" => assert_eq!(std::fs::read(&lock_path).unwrap(), b"retained evidence"),
                "hardlink" => {
                    let file = File::open(&lock_path).unwrap();
                    assert_eq!(windows_file_link_count(&file).unwrap(), 2);
                    assert_eq!(std::fs::read(&sentinel).unwrap(), b"");
                }
                _ => assert_eq!(
                    std::fs::read(lock_path.join("evidence")).unwrap(),
                    b"retained evidence"
                ),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    struct ControlParents {
        _root: tempfile::TempDir,
        active: PathBuf,
        original: PathBuf,
        replacement: PathBuf,
        primary: PathBuf,
        retained: File,
    }

    impl ControlParents {
        fn new(existing_controls: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let physical_root = root.path().canonicalize().unwrap();
            let active = physical_root.join("active");
            let replacement = physical_root.join("replacement");
            std::fs::create_dir(&active).unwrap();
            std::fs::create_dir(&replacement).unwrap();
            let primary = active.join("mail.sqlite3");
            std::fs::write(&primary, b"admitted mailbox").unwrap();
            std::fs::write(replacement.join("mail.sqlite3"), b"foreign mailbox").unwrap();
            if existing_controls {
                let breaker = replacement.join("mail.sqlite3.am-recovery-breaker.lock");
                std::fs::write(&breaker, b"foreign election evidence").unwrap();
                std::fs::set_permissions(&breaker, std::fs::Permissions::from_mode(0o666)).unwrap();
                let snapshot = replacement.join("mail.sqlite3.bak.authority.lock");
                std::fs::write(&snapshot, b"").unwrap();
                std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            let retained = open_directory(&active).unwrap();
            Self {
                _root: root,
                active,
                original: physical_root.join("original"),
                replacement,
                primary,
                retained,
            }
        }

        fn replace(&self) {
            std::fs::rename(&self.active, &self.original).unwrap();
            std::fs::rename(&self.replacement, &self.active).unwrap();
        }

        fn restore(&self) {
            std::fs::rename(&self.active, &self.replacement).unwrap();
            std::fs::rename(&self.original, &self.active).unwrap();
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ControlEntryWitness {
        device: u64,
        inode: u64,
        mode: u32,
        links: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        bytes: Vec<u8>,
    }

    fn control_namespace_witness(
        parent: &Path,
    ) -> std::collections::BTreeMap<OsString, ControlEntryWitness> {
        std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
                assert!(metadata.is_file());
                (
                    entry.file_name(),
                    ControlEntryWitness {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        mode: metadata.mode(),
                        links: metadata.nlink(),
                        modified: (metadata.mtime(), metadata.mtime_nsec()),
                        changed: (metadata.ctime(), metadata.ctime_nsec()),
                        bytes: std::fs::read(entry.path()).unwrap(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn snapshot_control_refuses_a_captured_foreign_parent_even_after_restoration() {
        for existing_controls in [false, true] {
            for restore_before_compare in [false, true] {
                let parents = ControlParents::new(existing_controls);
                let foreign = control_namespace_witness(&parents.replacement);
                parents.replace();
                let error = SnapshotAuthority::acquire_in_parent_with(
                    &parents.primary,
                    true,
                    &parents.retained,
                    || {
                        if restore_before_compare {
                            parents.restore();
                            ensure_same_directory(
                                &open_directory(&parents.active).unwrap(),
                                &parents.retained,
                            )
                            .unwrap();
                        }
                    },
                    || {},
                )
                .err()
                .expect("a restored spelling must not transfer captured directory authority");
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                if !restore_before_compare {
                    assert_eq!(control_namespace_witness(&parents.active), foreign);
                    parents.restore();
                }
                assert_eq!(control_namespace_witness(&parents.replacement), foreign);
                assert_eq!(std::fs::read_dir(&parents.active).unwrap().count(), 1);
                let authority =
                    SnapshotAuthority::acquire_in_parent(&parents.primary, true, &parents.retained)
                        .unwrap();
                authority.revalidate().unwrap();
                assert_eq!(
                    std::fs::read(&parents.primary).unwrap(),
                    b"admitted mailbox"
                );
            }
        }
    }

    #[test]
    fn snapshot_control_creation_stays_in_the_bound_parent_after_a_late_swap() {
        for existing_controls in [false, true] {
            let parents = ControlParents::new(existing_controls);
            let foreign = control_namespace_witness(&parents.replacement);
            let error = SnapshotAuthority::acquire_in_parent_with(
                &parents.primary,
                true,
                &parents.retained,
                || {},
                || parents.replace(),
            )
            .err()
            .expect("a displaced namespace must refuse admission");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(control_namespace_witness(&parents.active), foreign);
            let original_control = parents.original.join("mail.sqlite3.bak.authority.lock");
            let metadata = std::fs::symlink_metadata(&original_control).unwrap();
            assert!(metadata.is_file());
            assert_eq!(metadata.len(), 0);
            assert_eq!(metadata.nlink(), 1);
            assert_eq!(metadata.mode() & 0o777, 0o600);
            parents.restore();
            let authority =
                SnapshotAuthority::acquire_in_parent(&parents.primary, true, &parents.retained)
                    .unwrap();
            assert_eq!(authority.file.metadata().unwrap().ino(), metadata.ino());
            authority.revalidate().unwrap();
            assert_eq!(control_namespace_witness(&parents.replacement), foreign);
        }
    }

    #[test]
    fn retention_control_pair_refuses_replacement_between_each_acquisition() {
        for existing_controls in [false, true] {
            for with_expected_parent in [false, true] {
                for replace_after_recovery in [false, true] {
                    let parents = ControlParents::new(existing_controls);
                    let foreign = control_namespace_witness(&parents.replacement);
                    let expected = with_expected_parent.then_some(&parents.retained);
                    let error = acquire_retention_controls_with(
                        &parents.primary,
                        expected,
                        || {
                            if !replace_after_recovery {
                                parents.replace();
                            }
                        },
                        || {
                            if replace_after_recovery {
                                parents.replace();
                            }
                        },
                    )
                    .err()
                    .expect("both election controls must belong to one captured parent");
                    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                    assert_eq!(control_namespace_witness(&parents.active), foreign);
                    assert_eq!(
                        std::fs::read_dir(&parents.original).unwrap().count(),
                        if replace_after_recovery { 2 } else { 1 }
                    );
                    assert!(
                        !parents
                            .original
                            .join("mail.sqlite3.bak.authority.lock")
                            .exists()
                    );
                    assert!(
                        !parents
                            .original
                            .join("mail.sqlite3.am-recovery-breaker.json")
                            .exists()
                    );
                    parents.restore();
                    let (recovery, snapshot) =
                        acquire_retention_controls_with(&parents.primary, expected, || {}, || {})
                            .unwrap();
                    recovery.revalidate().unwrap();
                    snapshot.revalidate().unwrap();
                    let contended = crate::recovery_breaker::try_acquire_file_lock_in_parent(
                        &parents.primary,
                        &parents.retained,
                    )
                    .err()
                    .unwrap();
                    assert_eq!(contended.kind(), io::ErrorKind::WouldBlock);
                    assert!(SnapshotAuthority::acquire(&parents.primary, false).is_err());
                    drop(snapshot);
                    drop(recovery);
                    acquire_retention_controls_with(&parents.primary, expected, || {}, || {})
                        .unwrap();
                    assert_eq!(control_namespace_witness(&parents.replacement), foreign);
                    assert_eq!(
                        std::fs::read(&parents.primary).unwrap(),
                        b"admitted mailbox"
                    );
                }
            }
        }
    }

    #[test]
    fn shared_readers_coexist_and_exclude_publishers_without_unlinking() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().canonicalize().unwrap().join("mail.sqlite3");
        let first = SnapshotAuthority::acquire(&primary, false).unwrap();
        let inode = first.file.metadata().unwrap().ino();
        let second = SnapshotAuthority::acquire(&primary, false).unwrap();
        assert_eq!(
            SnapshotAuthority::acquire(&primary, true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        drop(second);
        let writer = SnapshotAuthority::acquire(&primary, true).unwrap();
        assert_eq!(writer.file.metadata().unwrap().ino(), inode);
        assert!(SnapshotAuthority::acquire(&primary, false).is_err());
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        let other = primary.with_file_name("other.sqlite3");
        let independent = SnapshotAuthority::acquire(&other, true).unwrap();
        independent.revalidate().unwrap();
        drop(writer);
        assert_eq!(
            SnapshotAuthority::acquire(&primary, true)
                .unwrap()
                .file
                .metadata()
                .unwrap()
                .ino(),
            inode
        );
    }

    #[test]
    fn unsafe_lock_shapes_are_preserved_and_refused() {
        for shape in ["symlink", "hardlink", "directory", "bytes", "public"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let lock = root.join("mail.sqlite3.bak.authority.lock");
            let sentinel = root.join("sentinel");
            std::fs::write(&sentinel, b"irreplaceable evidence").unwrap();
            match shape {
                "symlink" => std::os::unix::fs::symlink(&sentinel, &lock).unwrap(),
                "directory" => std::fs::create_dir(&lock).unwrap(),
                "hardlink" => std::fs::hard_link(&sentinel, &lock).unwrap(),
                _ => {
                    std::fs::write(
                        &lock,
                        if shape == "bytes" {
                            b"occupied".as_slice()
                        } else {
                            b""
                        },
                    )
                    .unwrap();
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &lock,
                        std::fs::Permissions::from_mode(if shape == "public" {
                            0o644
                        } else {
                            0o600
                        }),
                    )
                    .unwrap();
                }
            }
            assert!(
                SnapshotAuthority::acquire(&primary, true).is_err(),
                "{shape}"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"irreplaceable evidence");
            assert!(std::fs::symlink_metadata(&lock).is_ok());
        }
    }

    #[test]
    fn namespace_replacement_is_not_permission_to_reacquire_or_delete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = root.join("snapshots");
        std::fs::create_dir(&directory).unwrap();
        let primary = directory.join("mail.sqlite3");
        let guard = SnapshotAuthority::acquire(&primary, true).unwrap();
        let original = directory.join("mail.sqlite3.bak.authority.lock");
        let displaced = root.join("retained-lock");
        std::fs::rename(&original, &displaced).unwrap();
        assert!(guard.revalidate().is_err());
        std::fs::rename(&displaced, &original).unwrap();
        guard.revalidate().unwrap();
        std::fs::rename(&directory, root.join("retained-directory")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(guard.revalidate().is_err());
        drop(guard);
        assert!(
            root.join("retained-directory/mail.sqlite3.bak.authority.lock")
                .exists()
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    }

    #[test]
    fn retention_parent_binding_refuses_replacement_before_control_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = root.join("mailbox");
        let original = root.join("original");
        let replacement = root.join("replacement");
        std::fs::create_dir(&directory).unwrap();
        let primary = directory.join("mail.sqlite3");
        let expected = open_directory(&directory).unwrap();
        std::fs::rename(&directory, &original).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let sentinel = directory.join("unrelated-owner");
        std::fs::write(&sentinel, b"replacement directory evidence").unwrap();

        let error = SnapshotRetentionGuard::acquire_for_parent(&primary, &expected)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&original).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"replacement directory evidence"
        );

        std::fs::rename(&directory, &replacement).unwrap();
        std::fs::rename(&original, &directory).unwrap();
        let guard = SnapshotRetentionGuard::acquire_for_parent(&primary, &expected).unwrap();
        // The guard retains the admission identity after the caller drops its
        // handle, and still refuses a later replacement of the pathname.
        drop(expected);
        guard.revalidate().unwrap();
        std::fs::rename(&directory, &original).unwrap();
        std::fs::rename(&replacement, &directory).unwrap();
        assert!(guard.revalidate().is_err());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"replacement directory evidence"
        );
    }

    #[test]
    fn retention_parent_binding_compares_captured_descriptors_after_path_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = root.join("mailbox");
        let original = root.join("original");
        let replacement = root.join("replacement");
        std::fs::create_dir(&directory).unwrap();
        let expected = open_directory(&directory).unwrap();
        let primary = directory.join("mail.sqlite3");
        std::fs::rename(&directory, &original).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let captured = SnapshotAuthority::acquire(&primary, true).unwrap();
        captured.revalidate().unwrap();
        assert!(captured.ensure_parent(&expected).is_err());

        std::fs::rename(&directory, &replacement).unwrap();
        std::fs::rename(&original, &directory).unwrap();
        ensure_same_directory(&open_directory(&directory).unwrap(), &expected).unwrap();
        assert!(captured.ensure_parent(&expected).is_err());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        assert!(
            replacement
                .join("mail.sqlite3.bak.authority.lock")
                .is_file()
        );

        let ordinary = File::create(directory.join("ordinary-file")).unwrap();
        assert!(ensure_same_directory(&ordinary, &ordinary).is_err());
    }

    #[test]
    fn retention_excludes_recovery_for_the_entire_candidate_construction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let primary = root.join("mail.sqlite3");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let recovery_primary = primary.clone();
        let recovery = std::thread::spawn(move || {
            let _bypass = crate::recovery_breaker::RecoveryBreakerBypassGuard::enter();
            crate::pool::with_automatic_recovery_admission(
                &recovery_primary,
                "retention concurrency fixture",
                || -> io::Result<()> {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let refused = SnapshotRetentionGuard::acquire(&primary).err().unwrap();
        release_tx.send(()).unwrap();
        recovery.join().unwrap().unwrap();
        assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);

        let retention = SnapshotRetentionGuard::acquire(&primary).unwrap();
        let staged = root.join("candidate-construction-must-not-run");
        let candidate = staged.clone();
        let recovery_primary = primary.clone();
        let refused = std::thread::spawn(move || {
            let _bypass = crate::recovery_breaker::RecoveryBreakerBypassGuard::enter();
            crate::pool::with_automatic_recovery_admission(
                &recovery_primary,
                "retention construction contender",
                || std::fs::write(&candidate, b"candidate"),
            )
        })
        .join()
        .unwrap();
        assert!(matches!(
            refused,
            Err(crate::pool::AutomaticRecoveryRunError::Admission { .. })
        ));
        assert!(
            !staged.exists(),
            "recovery must refuse before constructing a candidate"
        );
        retention.revalidate().unwrap();
        drop(retention);
        crate::recovery_breaker::try_acquire_file_lock(&primary).unwrap();
    }

    #[test]
    fn refused_snapshot_admission_releases_the_recovery_election() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().canonicalize().unwrap().join("mail.sqlite3");
        let _reader = SnapshotAuthority::acquire(&primary, false).unwrap();
        assert!(SnapshotRetentionGuard::acquire(&primary).is_err());
        crate::recovery_breaker::try_acquire_file_lock(&primary).unwrap();
        assert!(!crate::recovery_breaker::breaker_sidecar_path(&primary).exists());
    }

    fn retention_search_fixture(root: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
        let primary = root.join("mail.sqlite3");
        let connection = crate::CanonicalDbConn::open_file(primary.display().to_string()).unwrap();
        connection
            .execute_raw(&crate::schema::init_schema_sql_base())
            .unwrap();
        drop(connection);
        let backup = super::super::snapshot_bak_path(&primary);
        std::fs::copy(&primary, &backup).unwrap();
        super::super::record_snapshot_metadata(&primary, 2_000_000_000_000_000).unwrap();
        let original = std::fs::read(&backup).unwrap();
        let pin = root.join("mail.sqlite3.bak.20000101_000000");
        std::fs::rename(&backup, &pin).unwrap();
        let mut different = original.clone();
        *different.last_mut().unwrap() ^= 0xff;
        std::fs::write(backup, different).unwrap();
        (primary, pin, original)
    }

    #[test]
    fn bounded_retention_search_resumes_and_reverifies_the_last_successful_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (primary, pin, original) = retention_search_fixture(&root);
        let current = super::super::snapshot_bak_path(&primary);
        for day in [2, 3] {
            std::fs::copy(
                &current,
                root.join(format!("mail.sqlite3.bak.2000010{day}_000000")),
            )
            .unwrap();
        }
        let metadata_path = super::super::snapshot_meta_path(&primary);
        let metadata = std::fs::read(&metadata_path).unwrap();
        let one_candidate = || RetentionSearchBudget {
            candidate_limit: 1,
            byte_limit: u64::MAX,
            ..RetentionSearchBudget::new(Instant::now() + std::time::Duration::from_secs(60))
        };
        for _ in 0..3 {
            let error =
                SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(one_candidate()))
                    .err()
                    .expect("the first three images have different bytes");
            assert_eq!(
                SnapshotRetentionDeferred::from_io_error(&error),
                Some(SnapshotRetentionDeferred::Candidates)
            );
            // A deferred search releases both real locks. It does not pin
            // generations indefinitely while the maintenance worker sleeps.
            crate::recovery_breaker::try_acquire_file_lock(&primary).unwrap();
            assert_eq!(std::fs::read(&pin).unwrap(), original);
        }
        for _ in 0..2 {
            let guard =
                SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(one_candidate()))
                    .unwrap();
            assert_eq!(guard.verified_source_path(), Some(pin.as_path()));
            guard.revalidate().unwrap();
        }
        // The successful path hint avoids repeatedly searching the earlier
        // images, but never caches verification. Changed bytes must fail.
        std::fs::copy(&current, &pin).unwrap();
        let error =
            SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(one_candidate()))
                .err()
                .expect("a path hint cannot authorize changed snapshot bytes");
        assert_eq!(
            SnapshotRetentionDeferred::from_io_error(&error),
            Some(SnapshotRetentionDeferred::Candidates)
        );
        assert_eq!(std::fs::read(&metadata_path).unwrap(), metadata);
        assert_eq!(std::fs::read(&primary).unwrap(), original);
    }

    #[test]
    fn retention_search_defers_time_count_and_bytes_without_losing_recovery_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (primary, pin, original) = retention_search_fixture(&root);
        let fresh =
            || RetentionSearchBudget::new(Instant::now() + std::time::Duration::from_secs(60));
        for (budget, expected) in [
            (
                RetentionSearchBudget::new(Instant::now()),
                SnapshotRetentionDeferred::Deadline,
            ),
            (
                RetentionSearchBudget {
                    candidate_limit: 0,
                    ..fresh()
                },
                SnapshotRetentionDeferred::Candidates,
            ),
            (
                RetentionSearchBudget {
                    byte_limit: 0,
                    ..fresh()
                },
                SnapshotRetentionDeferred::Bytes,
            ),
        ] {
            let error = SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(budget))
                .err()
                .expect("an incomplete search must be typed deferral");
            assert_eq!(
                SnapshotRetentionDeferred::from_io_error(&error),
                Some(expected)
            );
            crate::recovery_breaker::try_acquire_file_lock(&primary).unwrap();
            assert_eq!(std::fs::read(&pin).unwrap(), original);
        }
        let guard = SnapshotRetentionGuard::acquire(&primary).unwrap();
        assert_eq!(guard.verified_source_path(), Some(pin.as_path()));
    }

    #[test]
    fn bounded_search_completes_more_than_one_hundred_twenty_eight_nonmatching_images() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (primary, pin, original) = retention_search_fixture(&root);
        std::fs::rename(&pin, root.join("preserved-verified-bytes")).unwrap();
        let current = super::super::snapshot_bak_path(&primary);
        for index in 1..=130 {
            std::fs::copy(
                &current,
                root.join(format!(
                    "mail.sqlite3.bak.200001{:02}_{:02}0000",
                    index / 24 + 1,
                    index % 24,
                )),
            )
            .unwrap();
        }
        let budget =
            || RetentionSearchBudget::new(Instant::now() + std::time::Duration::from_secs(60));
        let first = SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(budget()))
            .err()
            .expect("131 same-sized images require multiple bounded slices");
        assert_eq!(
            SnapshotRetentionDeferred::from_io_error(&first),
            Some(SnapshotRetentionDeferred::Candidates)
        );
        let complete =
            SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(budget())).unwrap();
        assert!(complete.completed_search_without_pin());
        assert!(complete.verified_source_path().is_none());
        assert!(
            complete
                .delete_superseded_generation(
                    &primary,
                    &current,
                    &std::fs::metadata(&current).unwrap()
                )
                .is_err()
        );
        drop(complete);
        // A later exporter can observe completion without redoing the entire
        // search, but the result still contains no deletion authority.
        let complete = SnapshotRetentionGuard::acquire_with_parent(
            &primary,
            None,
            Some(RetentionSearchBudget {
                candidate_limit: 0,
                ..budget()
            }),
        )
        .unwrap();
        assert!(complete.completed_search_without_pin());
        assert!(complete.verified_source_path().is_none());
        drop(complete);
        // An earlier source now matches. Its changed witness invalidates the
        // completed sweep and the real byte/integrity checks discover it.
        std::fs::write(&current, &original).unwrap();
        let restored = SnapshotRetentionGuard::acquire_with_parent(
            &primary,
            None,
            Some(RetentionSearchBudget {
                candidate_limit: 1,
                ..budget()
            }),
        )
        .unwrap();
        assert_eq!(restored.verified_source_path(), Some(current.as_path()));
        assert!(!restored.completed_search_without_pin());
    }

    #[test]
    fn pending_search_restarts_when_earlier_or_new_candidates_change() {
        for change in ["rewrite", "replacement", "new"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let (primary, pin, original) = retention_search_fixture(&root);
            std::fs::rename(&pin, root.join("preserved-verified-bytes")).unwrap();
            let current = super::super::snapshot_bak_path(&primary);
            let wrong = std::fs::read(&current).unwrap();
            std::fs::write(root.join("mail.sqlite3.bak.20000102_000000"), &wrong).unwrap();
            let budget = || RetentionSearchBudget {
                candidate_limit: 1,
                ..RetentionSearchBudget::new(Instant::now() + std::time::Duration::from_secs(60))
            };
            let first = SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(budget()))
                .err()
                .unwrap();
            assert_eq!(
                SnapshotRetentionDeferred::from_io_error(&first),
                Some(SnapshotRetentionDeferred::Candidates)
            );
            match change {
                "rewrite" => {
                    let modified = std::fs::metadata(&current).unwrap().modified().unwrap();
                    std::fs::write(&current, &original).unwrap();
                    File::open(&current)
                        .unwrap()
                        .set_times(std::fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                }
                "replacement" => {
                    std::fs::rename(&current, root.join("preserved-nonmatching-image")).unwrap();
                    std::fs::write(&current, &wrong).unwrap();
                }
                _ => std::fs::write(root.join("mail.sqlite3.bak.20000103_000000"), &wrong).unwrap(),
            }
            let resumed =
                SnapshotRetentionGuard::acquire_with_parent(&primary, None, Some(budget()));
            if change == "rewrite" {
                let guard = resumed.unwrap();
                assert_eq!(guard.verified_source_path(), Some(current.as_path()));
                assert!(!guard.completed_search_without_pin());
            } else {
                let error = resumed
                    .err()
                    .expect("a changed source set must restart, not complete an old suffix");
                assert_eq!(
                    SnapshotRetentionDeferred::from_io_error(&error),
                    Some(SnapshotRetentionDeferred::Candidates)
                );
            }
        }
    }

    #[test]
    fn legacy_recovery_references_refuse_retention_and_remain_intact() {
        for shape in ["live", "stale", "unknown", "directory", "dangling"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let guard = SnapshotRetentionGuard::acquire(&primary).unwrap();
            let legacy = root.join("mail.sqlite3.recovery.lock");
            match shape {
                "live" => std::fs::write(&legacy, std::process::id().to_string()).unwrap(),
                "stale" => std::fs::write(&legacy, b"0").unwrap(),
                "unknown" => std::fs::write(&legacy, b"incomplete owner").unwrap(),
                "directory" => std::fs::create_dir(&legacy).unwrap(),
                _ => std::os::unix::fs::symlink(root.join("missing-owner"), &legacy).unwrap(),
            }
            assert!(guard.revalidate().is_err(), "{shape}");
            drop(guard);
            assert!(
                SnapshotRetentionGuard::acquire(&primary).is_err(),
                "{shape}"
            );
            assert!(std::fs::symlink_metadata(&legacy).is_ok(), "{shape}");
            crate::recovery_breaker::try_acquire_file_lock(&primary).unwrap();
        }
    }

    #[test]
    fn pending_recovery_receipts_pin_generations_after_the_owner_exits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let primary = root.join("mail.sqlite3");
        let guard = SnapshotRetentionGuard::acquire(&primary).unwrap();
        let receipts = root.join(".mcp-agent-mail-recovery-receipts/mail.sqlite3");
        std::fs::create_dir_all(&receipts).unwrap();
        let pending = receipts.join("recovery-admission.receipt.pending");
        std::fs::write(&pending, b"interrupted promotion intent").unwrap();
        assert!(guard.revalidate().is_err());
        drop(guard);
        assert!(SnapshotRetentionGuard::acquire(&primary).is_err());
        assert_eq!(
            std::fs::read(&pending).unwrap(),
            b"interrupted promotion intent"
        );
        std::fs::rename(
            &pending,
            receipts.join("recovery-admission.receipt.aborted"),
        )
        .unwrap();
        drop(SnapshotRetentionGuard::acquire(&primary).unwrap());
    }

    #[test]
    fn verified_pin_revalidation_rejects_source_replacement_aliases_and_companions() {
        for change in ["replacement", "hardlink", "contents", "wal"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let connection =
                crate::CanonicalDbConn::open_file(primary.display().to_string()).unwrap();
            connection
                .execute_raw(&crate::schema::init_schema_sql_base())
                .unwrap();
            drop(connection);
            let backup = super::super::snapshot_bak_path(&primary);
            std::fs::copy(&primary, &backup).unwrap();
            super::super::record_snapshot_metadata(&primary, 1_000_000).unwrap();
            let guard = SnapshotRetentionGuard::acquire(&primary).unwrap();
            assert_eq!(guard.verified_source_path(), Some(backup.as_path()));
            let metadata_before =
                std::fs::read(super::super::snapshot_meta_path(&primary)).unwrap();
            match change {
                "replacement" => {
                    let original = root.join("preserved-pin");
                    std::fs::rename(&backup, &original).unwrap();
                    std::fs::copy(&original, &backup).unwrap();
                }
                "hardlink" => std::fs::hard_link(&backup, root.join("pin-alias")).unwrap(),
                "contents" => {
                    use std::io::{Seek as _, Write as _};
                    let original_mtime = std::fs::metadata(&backup).unwrap().modified().unwrap();
                    let mut writer = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&backup)
                        .unwrap();
                    writer.seek(std::io::SeekFrom::End(-1)).unwrap();
                    writer.write_all(b"x").unwrap();
                    writer
                        .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
                        .unwrap();
                }
                _ => std::fs::write(
                    crate::pool::sqlite_path_with_suffix(&backup, "-wal"),
                    b"live WAL",
                )
                .unwrap(),
            }
            assert!(guard.revalidate().is_err(), "{change}");
            assert!(backup.exists());
            assert_eq!(
                std::fs::read(super::super::snapshot_meta_path(&primary)).unwrap(),
                metadata_before
            );
        }
    }
}
