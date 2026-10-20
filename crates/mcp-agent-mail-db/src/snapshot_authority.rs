//! Stable per-primary admission for snapshot metadata and verified selection.
//!
//! This file is never unlinked on release. Its inode is the cooperating
//! processes' authority; a replaced or aliased lock is an error, not stale
//! evidence to delete. Admission is nonblocking, including within one process.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use fs2::FileExt as _;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

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
#[must_use]
pub struct SnapshotRetentionGuard {
    authority: SnapshotAuthority,
    primary: PathBuf,
    metadata_path: PathBuf,
    metadata: RetentionMetadata,
    selected: Option<super::SelectedVerifiedSnapshot>,
    selected_identity: Option<std::fs::Metadata>,
    // Drop recovery last, after releasing the snapshot authority and source.
    recovery: crate::recovery_breaker::RecoveryBreakerFileLock,
}

#[derive(PartialEq, Eq)]
enum RetentionMetadata {
    Absent,
    Bytes(Vec<u8>),
    Unavailable,
}

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

impl SnapshotRetentionGuard {
    /// Acquire without waiting, then select the exact verified generation.
    /// An unreadable or malformed sidecar stays occupied, never absent.
    ///
    /// # Errors
    /// Refuses recovery, live readers/publishers, pending recovery intents,
    /// unsafe lock paths, or changed authority.
    pub fn acquire(primary: &Path) -> io::Result<Self> {
        let primary = super::normalized_snapshot_primary(primary).map_err(io::Error::other)?;
        // Recovery owns this election before selecting a snapshot. Use the
        // same ordering, and never acquire recovery's outcome/backoff state.
        let recovery = crate::recovery_breaker::try_acquire_file_lock(&primary)?;
        let authority = SnapshotAuthority::acquire(&primary, true)?;
        ensure_no_recovery_references(&primary)?;
        let metadata_path = super::snapshot_meta_path(&primary);
        let metadata = retention_metadata(&metadata_path);
        let selected = if matches!(metadata, RetentionMetadata::Bytes(_)) {
            // Already exclusive: reacquiring the public selector's shared lock
            // would reject our own admission and discard a valid pin.
            super::select_verified_snapshot_inner(&primary)
        } else {
            None
        };
        let selected_identity = selected
            .as_ref()
            .map(|selected| selected.source.metadata())
            .transpose()?;
        let guard = Self {
            recovery,
            authority,
            primary,
            metadata_path,
            metadata,
            selected,
            selected_identity,
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

    /// Reclaim a strict, dated periodic generation only when a newer, fully
    /// verified snapshot remains available under this same exclusive admission.
    /// The caller selects the retention window; this boundary enforces safety.
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
        crate::recovery_retention::preflight_inventoried_file_readers(path, inventoried)?;
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
        crate::recovery_retention::delete_inventoried_file(path, inventoried)
    }

    /// Recheck ownership and metadata before another destructive operation.
    /// Participating writers are excluded; changed external evidence refuses
    /// further work rather than allowing a stale pin to authorize eviction.
    pub fn revalidate(&self) -> io::Result<()> {
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

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

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

impl SnapshotAuthority {
    pub(super) fn acquire(primary: &Path, exclusive: bool) -> io::Result<Self> {
        let parent_path = primary
            .parent()
            .ok_or_else(|| invalid("snapshot primary has no parent"))?
            .to_path_buf();
        let mut name = primary
            .file_name()
            .ok_or_else(|| invalid("snapshot primary has no leaf"))?
            .to_os_string();
        name.push(".bak.authority.lock");
        let parent = open_directory(&parent_path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
