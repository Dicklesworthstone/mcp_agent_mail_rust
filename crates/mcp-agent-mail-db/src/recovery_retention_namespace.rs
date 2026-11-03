//! Descriptor-bound namespace operations for recovery-debris consolidation.
//!
//! Consolidation never copies or deletes a source. The separate snapshot
//! reclamation primitive deletes only files admitted by snapshot authority.
//! Each path component is opened without
//! following user-controlled symlinks; rename and durability sync use the same
//! retained directory handles. A changed namespace after a completed move is
//! an error, not permission to retry or to overwrite a replacement pathname.

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

pub(super) const MAX_RECLAIM_MOVE_ATTEMPTS: u32 = 128;
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// One newly claimed quarantine, retained across all moves in a batch.
///
/// Dropping it releases the directory lease and closes the handle; it never
/// removes the directory or evidence. Empty-directory reclamation must acquire
/// this same lease, so a newly claimed batch remains live before its first move.
#[derive(Debug)]
pub struct ReclaimDirectory {
    file: File,
    path: PathBuf,
}

impl ReclaimDirectory {
    /// Claim a fresh, private, directory-synced quarantine without following
    /// user-controlled ancestor symlinks. An occupied leaf is never reused.
    pub fn claim(path: &Path) -> io::Result<Self> {
        claim_reclaim_directory(path)
    }

    /// Requested absolute spelling. A concurrent namespace change can make
    /// this spelling stale; all moves also validate the retained authority.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stage precisely the regular file selected by a prior inventory.
    ///
    /// Reject replacement, changed length/mtime, and hard-linked aliases before
    /// moving. Sync the retained source file and both participating directories.
    /// A post-move failure carries [`CompletedReclaimMove`] in the IO error;
    /// callers must not report it as still at the source or retry it blindly.
    pub fn stage_inventoried_file(
        &self,
        source: &Path,
        expected: &Metadata,
    ) -> io::Result<PathBuf> {
        move_into_with(
            source,
            self,
            Some(expected),
            rename_reclaim_entry,
            sync_reclaim_move_parents,
        )
    }
}

/// The namespace move completed, but subsequent validation or sync failed.
///
/// This is deliberately distinguishable from a refused move without parsing
/// an English diagnostic. The destination spelling may no longer name the
/// retained directory when another actor has renamed an ancestor.
#[derive(Debug)]
pub struct CompletedReclaimMove {
    requested_destination: PathBuf,
    failures: Vec<String>,
}

impl CompletedReclaimMove {
    #[must_use]
    pub fn from_io_error(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }

    #[must_use]
    pub fn requested_destination(&self) -> &Path {
        &self.requested_destination
    }
}

impl std::fmt::Display for CompletedReclaimMove {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "artifact rename completed for requested destination {}; directory durability is unconfirmed or namespace validation failed: {}; evidence is retained in the opened destination directory, whose pathname may have changed; do not retry or roll back this move",
            self.requested_destination.display(),
            self.failures.join("; ")
        )
    }
}

impl std::error::Error for CompletedReclaimMove {}

/// Size of the namespace entry that was removed.
///
/// Allocated bytes describe
/// the inode's blocks before unlink, not a measurement of filesystem free
/// space (snapshots, compression, and unrelated IO can change that separately).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimedFileBytes {
    pub logical_bytes: u64,
    pub allocated_bytes: Option<u64>,
    /// A superseded snapshot was unlinked while another process's reader
    /// inventory was inaccessible. Its open descriptors and mappings retain
    /// their bytes, but its pathname is gone. Physical reclamation is unknown;
    /// `allocated_bytes` is `None` in this case.
    pub reader_inventory_incomplete: bool,
}

/// Evidence carried from the no-open preflight through verified snapshot
/// admission. This is not a claim that an inaccessible process has no readers.
#[derive(Debug, Default, Clone, Copy)]
pub struct SnapshotReaderEvidence {
    incomplete: bool,
}

/// An unlink completed, but its directory durability could not be confirmed.
///
/// The caller must report this separately from both retained and durably
/// deleted files, and must never retry against the old pathname blindly.
#[derive(Debug)]
pub struct CompletedReclaimDelete {
    source: PathBuf,
    bytes: ReclaimedFileBytes,
    error: io::Error,
}

impl CompletedReclaimDelete {
    #[must_use]
    pub fn from_io_error(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    #[must_use]
    pub const fn bytes(&self) -> ReclaimedFileBytes {
        self.bytes
    }
}

impl std::fmt::Display for CompletedReclaimDelete {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "backup unlink completed for {}; directory durability is unconfirmed: {}; do not retry against this pathname without a new inventory",
            self.source.display(),
            self.error,
        )
    }
}

impl std::error::Error for CompletedReclaimDelete {}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Freeze relative spelling once. Do not canonicalize caller-controlled
/// ancestors: that would erase the symlinks this boundary needs to reject.
fn absolute_spelling(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(invalid("reclaim paths must not contain parent traversal"));
    }
    Ok(absolute)
}

#[cfg(target_os = "macos")]
fn system_alias_target(path: &Path) -> PathBuf {
    for (alias, target) in [
        ("/var", "/private/var"),
        ("/tmp", "/private/tmp"),
        ("/etc", "/private/etc"),
    ] {
        let alias = Path::new(alias);
        if let Ok(tail) = path.strip_prefix(alias)
            && mcp_agent_mail_core::disk::is_trusted_system_directory_alias(alias)
        {
            return Path::new(target).join(tail);
        }
    }
    path.to_path_buf()
}

/// Walk from the real filesystem root, retaining each parent while opening
/// its child. Newly created components are private from their first instant;
/// their entries are synced before descending, so an unsynced ancestor cannot
/// make a later completed reclaim disappear on a crash.
fn open_directory(path: &Path, create_missing: bool) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid("reclaim directory authority must be absolute"));
    }
    // Keep the caller's spelling for results, but walk the verified physical
    // target of the existing root-owned macOS alias exception.
    #[cfg(target_os = "macos")]
    let checked_path = system_alias_target(path);
    #[cfg(not(target_os = "macos"))]
    let checked_path = path;
    let mut directory =
        File::from(rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?);
    for component in checked_path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid("reclaim directory contains parent traversal"));
            }
        };
        let child = match rustix::fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(child) => child,
            Err(error) if create_missing && error == rustix::io::Errno::NOENT => {
                // Check durability support before creating anything here.
                directory.sync_all()?;
                match rustix::fs::mkdirat(&directory, name, Mode::RWXU) {
                    Ok(()) => directory.sync_all()?,
                    Err(error) if error == rustix::io::Errno::EXIST => {}
                    Err(error) => return Err(io::Error::from(error)),
                }
                rustix::fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?
            }
            Err(error) => return Err(io::Error::from(error)),
        };
        directory = File::from(child);
    }
    directory.sync_all()?;
    Ok(directory)
}

fn same_object(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && FileType::from_raw_mode(left.st_mode) == FileType::from_raw_mode(right.st_mode)
}

fn validate_directory(path: &Path, retained: &File) -> io::Result<()> {
    let current = open_directory(path, false)?;
    let current_stat = rustix::fs::fstat(&current).map_err(io::Error::from)?;
    let retained_stat = rustix::fs::fstat(retained).map_err(io::Error::from)?;
    if !same_object(&current_stat, &retained_stat) {
        return Err(invalid(format!(
            "reclaim directory identity changed at {}",
            path.display()
        )));
    }
    Ok(())
}

/// Claim a new leaf under an anchored, privately created ancestor chain.
/// An occupied final name is never reused, including a dangling symlink.
pub(super) fn claim_reclaim_directory(path: &Path) -> io::Result<ReclaimDirectory> {
    let path = absolute_spelling(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("quarantine has no leaf name"))?;
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("quarantine has no parent"))?;
    let parent = open_directory(parent_path, true)?;
    // Serialize the mkdir/open/lease window with empty-directory reclamation
    // by other databases sharing this quarantine parent.
    fs2::FileExt::try_lock_exclusive(&parent)?;
    validate_directory(parent_path, &parent)?;
    rustix::fs::mkdirat(&parent, name, Mode::RWXU).map_err(io::Error::from)?;
    let file = File::from(
        rustix::fs::openat(&parent, name, DIRECTORY_FLAGS, Mode::empty())
            .map_err(io::Error::from)?,
    );
    fs2::FileExt::try_lock_exclusive(&file)?;
    // Preserve the newly created directory on every error. No cleanup by
    // pathname is safe once the namespace might have been replaced.
    parent.sync_all()?;
    file.sync_all()?;
    validate_directory(&path, &file)?;
    Ok(ReclaimDirectory { file, path })
}

pub(super) fn sync_reclaim_move_parents(
    source_parent: &File,
    destination_parent: &File,
) -> io::Result<()> {
    let destination_result = destination_parent.sync_all();
    let source_result = source_parent.sync_all();
    destination_result.and(source_result)
}

/// The primitive receives only retained directories and single-component
/// names. There is deliberately no CWD/pathname or copy/unlink fallback.
pub(super) fn rename_reclaim_entry(
    source_parent: &File,
    source_name: &OsStr,
    destination_parent: &File,
    destination_name: &OsStr,
) -> io::Result<()> {
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    ))]
    {
        rustix::fs::renameat_with(
            source_parent,
            source_name,
            destination_parent,
            destination_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    )))]
    {
        let _ = (
            source_parent,
            source_name,
            destination_parent,
            destination_name,
        );
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative atomic no-replace rename is unavailable",
        ))
    }
}

fn validate_source_entry(parent: &File, name: &OsStr, expected: &Stat) -> io::Result<()> {
    let current =
        rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    // A directory's size is no part of its identity: xfs keeps the parent's
    // inode number inside a small directory, in 4 or 8 bytes, so a rename
    // between parents on either side of 2^32 resizes it.
    let directory = FileType::from_raw_mode(expected.st_mode) == FileType::Directory;
    if !same_object(&current, expected) || (!directory && current.st_size != expected.st_size) {
        return Err(invalid(
            "reclaim source identity or size changed before publication",
        ));
    }
    Ok(())
}

fn validate_inventoried_file(file: &File, expected: &Metadata) -> io::Result<()> {
    let current = file.metadata()?;
    if !expected.file_type().is_file()
        || !current.file_type().is_file()
        || expected.nlink() != 1
        || current.nlink() != 1
        || expected.dev() != current.dev()
        || expected.ino() != current.ino()
        || expected.len() != current.len()
        || expected.mtime() != current.mtime()
        || expected.mtime_nsec() != current.mtime_nsec()
    {
        return Err(invalid(
            "rotation source no longer matches its inventoried regular, single-link file",
        ));
    }
    Ok(())
}

fn validate_delete_inventory(file: &File, expected: &Metadata) -> io::Result<()> {
    validate_inventoried_file(file, expected)?;
    let current = file.metadata()?;
    // Unlike a move, unlink admission has no intended inode metadata change.
    // ctime catches in-place edits even if an external writer restores mtime.
    if expected.ctime() != current.ctime() || expected.ctime_nsec() != current.ctime_nsec() {
        return Err(invalid("backup changed after deletion inventory"));
    }
    Ok(())
}

/// Remove an inventoried, empty native rotation directory.
///
/// The caller owns naming/policy admission; this boundary refuses live
/// leases, open readers, replaced inodes and symlinked ancestors. `rmdir`
/// atomically refuses any child added after inventory, so this can never
/// remove recovery evidence.
pub fn delete_empty_reclaim_directory(source: &Path, inventoried: &Metadata) -> io::Result<()> {
    delete_empty_reclaim_directory_with(
        source,
        inventoried,
        |parent, name| {
            rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
        },
        File::sync_all,
    )
}

fn delete_empty_reclaim_directory_with<U, S>(
    source: &Path,
    inventoried: &Metadata,
    mut unlink: U,
    mut sync: S,
) -> io::Result<()>
where
    U: FnMut(&File, &OsStr) -> io::Result<()>,
    S: FnMut(&File) -> io::Result<()>,
{
    let source = absolute_spelling(source)?;
    let parent_path = source
        .parent()
        .ok_or_else(|| invalid("reclaim directory has no parent"))?;
    let name = source
        .file_name()
        .ok_or_else(|| invalid("reclaim directory has no name"))?;
    let parent = open_directory(parent_path, false)?;
    fs2::FileExt::try_lock_exclusive(&parent)?;
    let directory = File::from(rustix::fs::openat(
        &parent,
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
    )?);
    let current = directory.metadata()?;
    if !inventoried.file_type().is_dir()
        || inventoried.dev() != current.dev()
        || inventoried.ino() != current.ino()
    {
        return Err(invalid("empty rotation directory changed after inventory"));
    }
    fs2::FileExt::try_lock_exclusive(&directory)?;
    ensure_no_other_file_readers(&source, &directory)?;
    let expected = rustix::fs::fstat(&directory)?;
    sync(&parent)?;
    validate_directory(parent_path, &parent)?;
    validate_source_entry(&parent, name, &expected)?;
    unlink(&parent, name)?;
    if let Err(error) = sync(&parent).and_then(|()| validate_directory(parent_path, &parent)) {
        return Err(io::Error::other(CompletedReclaimDelete {
            source,
            // Directory removal is accounted separately from payload bytes.
            bytes: ReclaimedFileBytes::default(),
            error,
        }));
    }
    Ok(())
}

/// Remove one admitted superseded snapshot through its retained parent. Never
/// follows symlinks, traverses directories, or accepts hard-linked aliases.
pub fn delete_inventoried_file(
    source: &Path,
    inventoried: &Metadata,
) -> io::Result<ReclaimedFileBytes> {
    delete_inventoried_file_with(
        source,
        inventoried,
        |parent, name| {
            rustix::fs::unlinkat(parent, name, AtFlags::empty()).map_err(io::Error::from)
        },
        File::sync_all,
    )
}

fn delete_inventoried_file_with<U, S>(
    source: &Path,
    inventoried: &Metadata,
    mut unlink: U,
    mut sync: S,
) -> io::Result<ReclaimedFileBytes>
where
    U: FnMut(&File, &OsStr) -> io::Result<()>,
    S: FnMut(&File) -> io::Result<()>,
{
    delete_inventoried_file_with_evidence(source, inventoried, None, &mut unlink, &mut sync)
}

/// Only snapshot authority may use this boundary, after verifying a newer
/// recoverable image, lineage, standalone SQLite state, and unchanged pins.
/// POSIX unlink leaves any unobservable reader's inode intact; it never
/// truncates, overwrites, or reuses that reader's bytes.
pub fn delete_superseded_snapshot_file(
    source: &Path,
    inventoried: &Metadata,
    readers: SnapshotReaderEvidence,
) -> io::Result<ReclaimedFileBytes> {
    delete_inventoried_file_with_evidence(
        source,
        inventoried,
        Some(readers),
        |parent, name| {
            rustix::fs::unlinkat(parent, name, AtFlags::empty()).map_err(io::Error::from)
        },
        File::sync_all,
    )
}

fn delete_inventoried_file_with_evidence<U, S>(
    source: &Path,
    inventoried: &Metadata,
    readers: Option<SnapshotReaderEvidence>,
    mut unlink: U,
    mut sync: S,
) -> io::Result<ReclaimedFileBytes>
where
    U: FnMut(&File, &OsStr) -> io::Result<()>,
    S: FnMut(&File) -> io::Result<()>,
{
    let source = absolute_spelling(source)?;
    let parent_path = source
        .parent()
        .ok_or_else(|| invalid("backup has no parent"))?;
    let name = source
        .file_name()
        .ok_or_else(|| invalid("backup has no name"))?;
    let parent = open_directory(parent_path, false)?;
    let file = File::from(rustix::fs::openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    validate_delete_inventory(&file, inventoried)?;
    // Snapshot admission excludes cooperating readers throughout this call.
    // Also refuse observed readers and advisory file locks. Only verified
    // superseded snapshots may tolerate an unobservable external process;
    // generic file/directory reclamation retains its strict same-owner policy.
    fs2::FileExt::try_lock_exclusive(&file)?;
    let reader_inventory_incomplete = if let Some(preflight) = readers {
        use std::os::fd::AsRawFd as _;

        let final_scan =
            inspect_snapshot_file_readers(&source, &file.metadata()?, Some(file.as_raw_fd()))?;
        preflight.incomplete || final_scan.incomplete
    } else {
        ensure_no_other_file_readers(&source, &file)?;
        false
    };
    let expected = rustix::fs::fstat(&file)?;
    sync(&parent)?;
    validate_directory(parent_path, &parent)?;
    validate_source_entry(&parent, name, &expected)?;
    validate_delete_inventory(&file, inventoried)?;
    #[cfg(target_os = "linux")]
    if reader_inventory_incomplete {
        let caller = read_process_status_summary(Path::new("/proc/thread-self"))?;
        validate_opaque_snapshot_scope(&source, inventoried, caller.uids[3])?;
    }
    let metadata = file.metadata()?;
    let bytes = ReclaimedFileBytes {
        logical_bytes: metadata.len(),
        allocated_bytes: (!reader_inventory_incomplete)
            .then(|| metadata.blocks().checked_mul(512))
            .flatten(),
        reader_inventory_incomplete,
    };
    unlink(&parent, name)?;
    drop(file);
    if let Err(error) = sync(&parent).and_then(|()| validate_directory(parent_path, &parent)) {
        return Err(io::Error::other(CompletedReclaimDelete {
            source,
            bytes,
            error,
        }));
    }
    Ok(bytes)
}

/// Check observable readers without opening the candidate at all.
///
/// Closing a fresh descriptor could release a same-process SQLite reader's POSIX locks,
/// so canonical verification must perform this preflight before its first open.
pub fn preflight_inventoried_file_readers(source: &Path, inventoried: &Metadata) -> io::Result<()> {
    ensure_no_file_readers(source, inventoried, None)
}

/// No-open preflight for snapshot authority's verified periodic lane. Own
/// process readers always refuse before canonical SQLite verification can
/// open/close the inode and accidentally release that process's POSIX locks.
pub fn preflight_superseded_snapshot_readers(
    source: &Path,
    inventoried: &Metadata,
) -> io::Result<SnapshotReaderEvidence> {
    inspect_snapshot_file_readers(source, inventoried, None)
}

#[cfg(target_os = "linux")]
fn inspect_snapshot_file_readers(
    source: &Path,
    inventoried: &Metadata,
    excluded_descriptor: Option<i32>,
) -> io::Result<SnapshotReaderEvidence> {
    inspect_file_readers(source, inventoried, excluded_descriptor, true)
}

#[cfg(not(target_os = "linux"))]
fn inspect_snapshot_file_readers(
    source: &Path,
    inventoried: &Metadata,
    excluded_descriptor: Option<i32>,
) -> io::Result<SnapshotReaderEvidence> {
    ensure_no_file_readers(source, inventoried, excluded_descriptor)?;
    Ok(SnapshotReaderEvidence::default())
}

fn ensure_no_other_file_readers(source: &Path, retained: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    ensure_no_file_readers(source, &retained.metadata()?, Some(retained.as_raw_fd()))
}

/// External readers do not necessarily participate in snapshot admission.
/// A strict scan requires same-owner processes to be observable. Snapshot
/// authority has a separate, explicit unlink-only policy for verified,
/// superseded regular files, recording denied inventories as uncertainty.
/// Neither policy atomically excludes arbitrary external opens.
#[cfg(target_os = "linux")]
fn is_proc_task_gone(error: &io::Error) -> bool {
    // Linux procfs returns ENOENT when `/proc/<pid>` vanishes before open and
    // ESRCH (errno 3, "No such process") when the task exits after a proc file
    // or directory handle was opened (`get_proc_task(inode) == NULL`).
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

/// A failure here is about another process, not the candidate under inspection,
/// so name the entry. The kind survives for callers that read it.
#[cfg(target_os = "linux")]
fn proc_entry_error(entry: &Path, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", entry.display()))
}

#[cfg(target_os = "linux")]
fn ensure_no_file_readers(
    source: &Path,
    retained_identity: &Metadata,
    excluded_descriptor: Option<i32>,
) -> io::Result<()> {
    inspect_file_readers(source, retained_identity, excluded_descriptor, false).map(|_| ())
}

/// An actual, persistent procfs denial with readable kernel identity evidence
/// from the same pinned task. Other PermissionDenied errors (including unreadable
/// status/ownership) must never be downgraded to this classification.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct UnobservableReaderInventory(String);

#[cfg(target_os = "linux")]
impl std::fmt::Display for UnobservableReaderInventory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for UnobservableReaderInventory {}

#[cfg(target_os = "linux")]
fn is_unobservable_reader_inventory(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<UnobservableReaderInventory>)
}

#[cfg(target_os = "linux")]
fn inspect_file_readers(
    source: &Path,
    retained_identity: &Metadata,
    excluded_descriptor: Option<i32>,
    superseded_snapshot: bool,
) -> io::Result<SnapshotReaderEvidence> {
    const MAX_READER_INVENTORY_ENTRIES: usize = 131_072;
    let canonical_source = source.canonicalize()?;
    // `/proc` may be mounted from an ancestor PID namespace. getpid() then
    // names a different task in this mount (or no task at all), so only the
    // procfs spelling can identify our retained descriptor in this inventory.
    let own_process = std::fs::read_link("/proc/self")?;
    let own_pid = own_process
        .to_str()
        .and_then(|pid| pid.parse::<u32>().ok())
        .ok_or_else(|| invalid("open-reader inventory cannot identify /proc/self"))?;
    let self_status = read_process_status_summary(Path::new("/proc/thread-self"))?;
    let mut remaining = MAX_READER_INVENTORY_ENTRIES;
    let mut charge = || -> io::Result<()> {
        remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("open-reader inventory exceeded its bounded scan"))?;
        Ok(())
    };
    // Never hide a same-process reader behind an unrelated opaque task. The
    // current thread may have unshared its descriptor table from the leader.
    let own_thread = Path::new("/proc").join(std::fs::read_link("/proc/thread-self")?);
    inspect_process_reader_inventory(
        &own_thread,
        own_pid,
        &self_status,
        retained_identity,
        &canonical_source,
        excluded_descriptor,
        &mut charge,
    )?;
    // Linux POSIX locks belong to the current files_struct. The strict scan
    // above covers every thread sharing that descriptor table; closing our
    // verifier cannot release locks owned by an unshared table. Do not scan
    // the same descriptors and address space once per worker thread: that
    // would consume the bounded inventory solely because the server is busy.
    // The leader and any surviving workers are still checked below as before.
    let mut evidence = SnapshotReaderEvidence::default();
    for entry in std::fs::read_dir("/proc")? {
        charge()?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if is_proc_task_gone(&error) => continue,
            Err(error) => return Err(error),
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let result = inspect_process_reader_inventory(
            &entry.path(),
            pid,
            &self_status,
            retained_identity,
            &canonical_source,
            (pid == own_pid).then_some(excluded_descriptor).flatten(),
            &mut charge,
        );
        match result {
            Err(error)
                if superseded_snapshot
                    && pid != own_pid
                    && is_unobservable_reader_inventory(&error) =>
            {
                // Do not stop here: a later, readable process may be a known
                // reader. Bounds, malformed status and all other IO failures
                // remain refusals. This is uncertainty, never reader absence.
                evidence.incomplete = true;
            }
            result => result?,
        }
    }
    if evidence.incomplete {
        validate_opaque_snapshot_scope(source, retained_identity, self_status.uids[3])?;
    }
    Ok(evidence)
}

/// Limit the uncertain-reader policy to owned, non-shared, single-link files
/// in an owned directory that other users cannot modify. Snapshot authority
/// separately establishes provenance and recoverability before any unlink.
#[cfg(target_os = "linux")]
fn validate_opaque_snapshot_scope(
    source: &Path,
    inventoried: &Metadata,
    filesystem_uid: u32,
) -> io::Result<()> {
    let source = absolute_spelling(source)?;
    let current = std::fs::symlink_metadata(&source)?;
    let parent = open_directory(
        source
            .parent()
            .ok_or_else(|| invalid("backup has no parent"))?,
        false,
    )?;
    let parent = parent.metadata()?;
    if !current.is_file()
        || !inventoried.is_file()
        || current.nlink() != 1
        || inventoried.nlink() != 1
        || current.dev() != inventoried.dev()
        || current.ino() != inventoried.ino()
        || current.uid() != filesystem_uid
        || inventoried.uid() != filesystem_uid
        || current.mode() & 0o022 != 0
        || parent.uid() != filesystem_uid
        || parent.mode() & 0o022 != 0
    {
        return Err(invalid(
            "unobservable readers require an owned, non-shared, single-link snapshot namespace",
        ));
    }
    Ok(())
}

/// Retry an actual denied inventory once, not just the process's status.
/// Dumpability can change during exec or exit; a readable status record does
/// not say whether the fd/maps operation that failed is now permitted. A
/// second denial remains a refusal, including for an ordinary session daemon.
#[cfg(target_os = "linux")]
fn retry_process_reader_scan(
    mut scan: impl FnMut() -> io::Result<()>,
    mut pause: impl FnMut(),
) -> io::Result<()> {
    match scan() {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            pause();
            scan()
        }
        result => result,
    }
}

#[cfg(target_os = "linux")]
fn inspect_process_reader_inventory(
    process: &Path,
    process_id: u32,
    self_status: &ProcessStatusSummary,
    retained_identity: &Metadata,
    canonical_source: &Path,
    excluded_descriptor: Option<i32>,
    mut charge: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    inspect_pinned_process_reader_inventory(
        process,
        process_id,
        self_status,
        retained_identity,
        canonical_source,
        excluded_descriptor,
        &mut charge,
    )
}

#[cfg(target_os = "linux")]
fn inspect_pinned_process_reader_inventory(
    process: &Path,
    process_id: u32,
    self_status: &ProcessStatusSummary,
    retained_identity: &Metadata,
    canonical_source: &Path,
    excluded_descriptor: Option<i32>,
    mut charge: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // A proc directory descriptor stays bound to the original task after
    // exit; reopening the numeric pathname could instead inspect a reused
    // PID on the retry. O_PATH requires no access to the protected fd table.
    // Use thread-self because the caller's thread can have its own fd table.
    let directory = match rustix::fs::open(
        process,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(error) if is_proc_task_gone(&io::Error::from(error)) => return Ok(()),
        Err(error) => return Err(proc_entry_error(process, &io::Error::from(error))),
    };
    let anchored = PathBuf::from(format!("/proc/thread-self/fd/{}", directory.as_raw_fd()));
    let result = retry_process_reader_scan(
        || {
            let mut unobservable = None;
            record_reader_inventory_result(
                inspect_process_file_readers_once(
                    &anchored,
                    process_id,
                    self_status,
                    retained_identity,
                    canonical_source,
                    excluded_descriptor,
                    &mut charge,
                ),
                &mut unobservable,
            )?;
            // An exited leader can have empty fd/maps files while its workers
            // still retain the backup. Both scans use the same pinned task.
            record_reader_inventory_result(
                inspect_surviving_thread_readers(
                    &anchored,
                    process_id,
                    self_status,
                    retained_identity,
                    canonical_source,
                    &mut charge,
                ),
                &mut unobservable,
            )?;
            unobservable.map_or(Ok(()), Err)
        },
        || std::thread::sleep(std::time::Duration::from_millis(5)),
    );
    // Keep operator diagnostics in /proc/<pid> terms, not this short-lived
    // internal descriptor spelling. This translation never decides admission.
    result.map_err(|error| {
        let unobservable = is_unobservable_reader_inventory(&error);
        let detail = error.to_string();
        let prefix = format!("{}/", anchored.display());
        if let Some(entry) = detail.strip_prefix(&prefix) {
            let detail = format!("{}/{entry}", process.display());
            if unobservable {
                io::Error::new(error.kind(), UnobservableReaderInventory(detail))
            } else {
                io::Error::new(error.kind(), detail)
            }
        } else {
            error
        }
    })
}

/// Classify only the task whose fd/maps operation was denied. A denial while
/// reading that task's identity is a hard error, not opaque-reader evidence.
#[cfg(target_os = "linux")]
fn unobservable_reader_inventory(
    process: &Path,
    entry: &Path,
    error: &io::Error,
) -> io::Result<()> {
    match read_process_status_summary(process) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            UnobservableReaderInventory(proc_entry_error(entry, error).to_string()),
        )),
        Err(gone) if is_proc_task_gone(&gone) => Ok(()),
        Err(status) => Err(proc_entry_error(&process.join("status"), &status)),
    }
}

/// Keep uncertainty while finishing the other observable inventory lanes.
/// A known reader, exhausted bound, or hard IO error always takes precedence.
#[cfg(target_os = "linux")]
fn record_reader_inventory_result(
    result: io::Result<()>,
    unobservable: &mut Option<io::Error>,
) -> io::Result<()> {
    match result {
        Err(error) if is_unobservable_reader_inventory(&error) => {
            unobservable.get_or_insert(error);
            Ok(())
        }
        result => result,
    }
}

#[cfg(target_os = "linux")]
fn inspect_process_file_readers_once(
    process: &Path,
    process_id: u32,
    self_status: &ProcessStatusSummary,
    retained_identity: &Metadata,
    canonical_source: &Path,
    excluded_descriptor: Option<i32>,
    mut charge: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut unobservable = None;
    let descriptors = match std::fs::read_dir(process.join("fd")) {
        Ok(descriptors) => Some(descriptors),
        Err(error) if is_proc_task_gone(&error) => return Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                && reader_inventory_denial_can_be_skipped(
                    process,
                    self_status,
                    retained_identity.uid(),
                )? =>
        {
            None
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            record_reader_inventory_result(
                unobservable_reader_inventory(process, &process.join("fd"), &error),
                &mut unobservable,
            )?;
            None
        }
        Err(error) => return Err(proc_entry_error(&process.join("fd"), &error)),
    };
    for descriptor in descriptors.into_iter().flatten() {
        charge()?;
        let descriptor = match descriptor {
            Ok(descriptor) => descriptor,
            Err(error) if is_proc_task_gone(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                record_reader_inventory_result(
                    unobservable_reader_inventory(process, &process.join("fd"), &error),
                    &mut unobservable,
                )?;
                continue;
            }
            Err(error) => return Err(proc_entry_error(&process.join("fd"), &error)),
        };
        if excluded_descriptor.is_some_and(|excluded| {
            descriptor
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                == Some(excluded)
        }) {
            continue;
        }
        let target = match std::fs::read_link(descriptor.path()) {
            Ok(target) => target,
            Err(error) if is_proc_task_gone(&error) => continue,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && reader_inventory_denial_can_be_skipped(
                        process,
                        self_status,
                        retained_identity.uid(),
                    )? =>
            {
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                record_reader_inventory_result(
                    unobservable_reader_inventory(process, &descriptor.path(), &error),
                    &mut unobservable,
                )?;
                continue;
            }
            Err(error) => return Err(proc_entry_error(&descriptor.path(), &error)),
        };
        // Non-filesystem descriptors (`socket:[...]`, `pipe:[...]`,
        // `anon_inode:[...]`) never reference a regular file inode on disk.
        if !target.is_absolute() {
            continue;
        }
        let same_inode = if target == canonical_source {
            true
        } else {
            match descriptor_references_inode(process, &descriptor, retained_identity) {
                Ok(same_inode) => same_inode,
                Err(error) if is_proc_task_gone(&error) => continue,
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && reader_inventory_denial_can_be_skipped(
                            process,
                            self_status,
                            retained_identity.uid(),
                        )? =>
                {
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    record_reader_inventory_result(
                        unobservable_reader_inventory(process, &descriptor.path(), &error),
                        &mut unobservable,
                    )?;
                    continue;
                }
                Err(error) => return Err(proc_entry_error(&descriptor.path(), &error)),
            }
        };
        if same_inode {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("backup has an active file reader in process/task {process_id}"),
            ));
        }
    }
    if retained_identity.file_type().is_file() {
        record_reader_inventory_result(
            ensure_no_mapped_file_readers(
                process,
                process_id,
                self_status,
                retained_identity,
                &mut charge,
            ),
            &mut unobservable,
        )?;
    }
    unobservable.map_or(Ok(()), Err)
}

#[cfg(target_os = "linux")]
fn inspect_surviving_thread_readers(
    process: &Path,
    process_id: u32,
    self_status: &ProcessStatusSummary,
    retained_identity: &Metadata,
    canonical_source: &Path,
    mut charge: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let summary = match read_process_status_summary(process) {
        Ok(summary) => summary,
        Err(error) if is_proc_task_gone(&error) => return Ok(()),
        Err(error) => return Err(proc_entry_error(&process.join("status"), &error)),
    };
    if summary.has_vm_size || summary.threads == 1 {
        return Ok(());
    }
    let tasks_path = process.join("task");
    let tasks = match std::fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if is_proc_task_gone(&error) => return Ok(()),
        Err(error) => return Err(proc_entry_error(&tasks_path, &error)),
    };
    let mut unobservable = None;
    for task in tasks {
        charge()?;
        let task = match task {
            Ok(task) => task,
            Err(error) if is_proc_task_gone(&error) => continue,
            Err(error) => return Err(proc_entry_error(&tasks_path, &error)),
        };
        let Some(task_id) = task
            .file_name()
            .to_str()
            .and_then(|id| id.parse::<u32>().ok())
        else {
            return Err(invalid("open-reader task inventory has an invalid task ID"));
        };
        if task_id == process_id {
            continue;
        }
        // Do not exclude the caller's descriptor number in another task's
        // table: Linux threads can unshare their descriptors. An ambiguous
        // self-reader is a refusal, never authority to ignore a real reader.
        record_reader_inventory_result(
            inspect_pinned_process_reader_inventory(
                &task.path(),
                task_id,
                self_status,
                retained_identity,
                canonical_source,
                None,
                &mut charge,
            ),
            &mut unobservable,
        )?;
    }
    unobservable.map_or(Ok(()), Err)
}

/// Mappings retain an inode after the process closes its last descriptor.
/// Match the kernel's device/inode record, including mappings through removed
/// aliases, without opening a candidate or relying on a displayed pathname.
#[cfg(target_os = "linux")]
fn ensure_no_mapped_file_readers(
    process: &Path,
    process_id: u32,
    self_status: &ProcessStatusSummary,
    retained: &Metadata,
    mut charge: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Read as _;

    const MAX_PROCESS_MAP_BYTES: u64 = 8 * 1024 * 1024;
    let maps_file = match File::open(process.join("maps")) {
        Ok(file) => file,
        Err(error) if is_proc_task_gone(&error) => return Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                && reader_inventory_denial_can_be_skipped(
                    process,
                    self_status,
                    retained.uid(),
                )? =>
        {
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return unobservable_reader_inventory(process, &process.join("maps"), &error);
        }
        Err(error) => return Err(proc_entry_error(&process.join("maps"), &error)),
    };
    let mut maps = Vec::with_capacity(64 * 1024);
    match maps_file
        .take(MAX_PROCESS_MAP_BYTES + 1)
        .read_to_end(&mut maps)
    {
        Ok(_) => {}
        Err(error) if is_proc_task_gone(&error) => return Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                && reader_inventory_denial_can_be_skipped(
                    process,
                    self_status,
                    retained.uid(),
                )? =>
        {
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return unobservable_reader_inventory(process, &process.join("maps"), &error);
        }
        Err(error) => return Err(proc_entry_error(&process.join("maps"), &error)),
    }
    if u64::try_from(maps.len()).unwrap_or(u64::MAX) > MAX_PROCESS_MAP_BYTES {
        return Err(invalid("open-reader mappings exceeded their bounded scan"));
    }
    for line in maps
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        charge()?;
        // Only numeric fields are UTF-8: a mapped pathname may contain any
        // non-NUL Unix bytes and is not part of this identity comparison.
        let mut fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let device = fields
            .nth(3)
            .ok_or_else(|| invalid("open-reader mapping has no device record"))?;
        let inode = fields
            .next()
            .ok_or_else(|| invalid("open-reader mapping has no inode record"))?;
        let inode = std::str::from_utf8(inode)
            .map_err(io::Error::other)?
            .parse::<u64>()
            .map_err(io::Error::other)?;
        if inode != retained.ino() {
            continue;
        }
        let device = std::str::from_utf8(device).map_err(io::Error::other)?;
        let (major, minor) = device
            .split_once(':')
            .ok_or_else(|| invalid("open-reader mapping has an invalid device record"))?;
        let major = u64::from_str_radix(major, 16).map_err(io::Error::other)?;
        let minor = u64::from_str_radix(minor, 16).map_err(io::Error::other)?;
        if major == nix::sys::stat::major(retained.dev())
            && minor == nix::sys::stat::minor(retained.dev())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("backup has an active memory mapping in process {process_id}"),
            ));
        }
    }
    Ok(())
}

/// `read_link` alone misses an open hard-link alias whose directory entry has
/// already been unlinked. Read the kernel's inode hint before a device/inode
/// stat, avoiding unrelated storage IO for descriptors on blocked filesystems.
#[cfg(target_os = "linux")]
fn descriptor_references_inode(
    process: &Path,
    descriptor: &std::fs::DirEntry,
    retained: &Metadata,
) -> io::Result<bool> {
    let inode = read_descriptor_inode(File::open(
        process.join("fdinfo").join(descriptor.file_name()),
    )?)?;
    if inode != retained.ino() {
        return Ok(false);
    }
    let observed = std::fs::metadata(descriptor.path())?;
    Ok(observed.dev() == retained.dev() && observed.ino() == retained.ino())
}

/// Procfs emits the inode before optional lock and descriptor-specific records.
/// Consume its complete line without allocating or issuing an extra EOF read
/// for every descriptor in every fresh inventory. Partial reads and EINTR do
/// not change the 4 KiB bound; an incomplete or malformed inode never admits
/// reclamation. This reads only procfs, without statting unrelated filesystems.
#[cfg(target_os = "linux")]
fn read_descriptor_inode(mut reader: impl io::Read) -> io::Result<u64> {
    let mut info = [0_u8; 4096];
    let mut used = 0;
    let mut line_start = 0;
    while used < info.len() {
        let read = match reader.read(&mut info[used..]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let before = used;
        used += read;
        for end in before..used {
            if info[end] != b'\n' {
                continue;
            }
            if let Some(inode) = info[line_start..end].strip_prefix(b"ino:") {
                return std::str::from_utf8(inode)
                    .map_err(io::Error::other)?
                    .trim()
                    .parse::<u64>()
                    .map_err(io::Error::other);
            }
            line_start = end + 1;
        }
    }
    Err(invalid(
        "open-reader descriptor has no bounded inode record",
    ))
}

/// A nondumpable process has root-owned proc entries even when its effective
/// user is the mailbox owner. Inspect the kernel's bounded status record before
/// treating an inaccessible process as another user's observer limitation.
#[cfg(target_os = "linux")]
struct ProcessStatusSummary {
    state: char,
    has_vm_size: bool,
    fd_size: u64,
    threads: u32,
    uids: [u32; 4],
}

#[cfg(target_os = "linux")]
impl ProcessStatusSummary {
    const fn is_exited_without_files_or_address_space(&self) -> bool {
        // These are kernel-owned facts, unlike a process's mutable name.
        // Once terminal, a task cannot acquire a new table or address space.
        // This certifies only this task: the caller still scans any surviving
        // workers of a terminated thread-group leader.
        matches!(self.state, 'Z' | 'X' | 'x') && !self.has_vm_size && self.fd_size == 0
    }
}

#[cfg(target_os = "linux")]
fn parse_status_id_quad(status: &[u8], prefix: &[u8], label: &str) -> io::Result<[u32; 4]> {
    let line = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| invalid(format!("open-reader process status has no {label} record")))?;
    let mut values = std::str::from_utf8(line)
        .map_err(io::Error::other)?
        .split_whitespace();
    let mut ids = [0; 4];
    for id in &mut ids {
        *id = values
            .next()
            .ok_or_else(|| invalid(format!("open-reader process {label} record is incomplete")))?
            .parse()
            .map_err(io::Error::other)?;
    }
    if values.next().is_some() {
        return Err(invalid(format!(
            "open-reader process {label} record is ambiguous"
        )));
    }
    Ok(ids)
}

#[cfg(target_os = "linux")]
fn read_process_status_summary(process: &Path) -> io::Result<ProcessStatusSummary> {
    use std::io::Read as _;

    const MAX_PROCESS_STATUS_BYTES: usize = 64 * 1024;
    let mut status = Vec::with_capacity(4096);
    File::open(process.join("status"))?
        .take(MAX_PROCESS_STATUS_BYTES as u64 + 1)
        .read_to_end(&mut status)?;
    if status.len() > MAX_PROCESS_STATUS_BYTES {
        return Err(invalid(
            "open-reader process status exceeded its bounded scan",
        ));
    }
    let state = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"State:"))
        .and_then(|line| std::str::from_utf8(line).ok())
        .and_then(|line| line.trim_start().chars().next())
        .ok_or_else(|| invalid("open-reader process status has no state"))?;
    let fd_size = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"FDSize:"))
        .and_then(|line| std::str::from_utf8(line).ok())
        .and_then(|line| line.trim().parse::<u64>().ok())
        .ok_or_else(|| invalid("open-reader process status has no descriptor-table size"))?;
    let threads = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Threads:"))
        .and_then(|line| std::str::from_utf8(line).ok())
        .and_then(|line| line.trim().parse::<u32>().ok())
        .filter(|threads| *threads > 0)
        .ok_or_else(|| invalid("open-reader process status has no valid thread count"))?;
    let has_vm_size = status
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"VmSize:"));
    let uids = parse_status_id_quad(&status, b"Uid:", "UID")?;
    Ok(ProcessStatusSummary {
        state,
        has_vm_size,
        fd_size,
        threads,
        uids,
    })
}

#[cfg(target_os = "linux")]
#[cfg(test)]
fn process_reader_uids(process: &Path) -> io::Result<[u32; 4]> {
    Ok(read_process_status_summary(process)?.uids)
}

#[cfg(target_os = "linux")]
fn status_summary_denial_is_foreign(
    summary: &ProcessStatusSummary,
    self_status: &ProcessStatusSummary,
    file_owner: u32,
) -> bool {
    // Names are mutable, and credential/capability changes do not close
    // inherited descriptors or mappings. A nondumpable session helper can
    // therefore still be a real reader, even with a different saved UID or
    // GID. Missing leader VmSize or a zombie leader likewise says nothing
    // about descriptors held by the rest of its thread group.
    //
    // Preserve the documented limitation for inaccessible *other-user*
    // processes, but do not mistake any caller or file-owner UID for one.
    !summary.uids.contains(&file_owner)
        && !summary
            .uids
            .iter()
            .any(|uid| self_status.uids.contains(uid))
}

#[cfg(target_os = "linux")]
fn reader_inventory_denial_can_be_skipped(
    process: &Path,
    self_status: &ProcessStatusSummary,
    file_owner: u32,
) -> io::Result<bool> {
    match read_process_status_summary(process) {
        Ok(summary) => Ok(
            status_summary_denial_is_foreign(&summary, self_status, file_owner)
                || summary.is_exited_without_files_or_address_space(),
        ),
        // A task that exited between denial and inspection no longer holds a
        // reader. A live opaque task or unavailable ownership evidence still
        // refuses; the enclosing inventory retries the actual scan once.
        Err(error) if is_proc_task_gone(&error) => Ok(true),
        Err(error) => Err(proc_entry_error(&process.join("status"), &error)),
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_no_file_readers(
    source: &Path,
    _retained_identity: &Metadata,
    excluded_descriptor: Option<i32>,
) -> io::Result<()> {
    let output = std::process::Command::new("lsof")
        .args(["-n", "-P", "-Fpf", "--"])
        .arg(source)
        .output()?;
    if excluded_descriptor.is_none()
        && output.status.code() == Some(1)
        && output.stdout.is_empty()
        && output.stderr.is_empty()
    {
        // lsof reports no matches with status 1. No descriptor was opened by
        // preflight itself, so an empty, otherwise clean inventory is expected.
        return Ok(());
    }
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(io::Error::other("open-reader inventory unavailable"));
    }
    let output = std::str::from_utf8(&output.stdout).map_err(io::Error::other)?;
    let mut current_pid = None;
    let mut observed_self = false;
    for line in output.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current_pid = Some(pid.parse::<u32>().map_err(io::Error::other)?);
        } else if let Some(descriptor) = line.strip_prefix('f') {
            if current_pid == Some(std::process::id())
                && excluded_descriptor
                    .is_some_and(|excluded| descriptor.parse::<i32>().ok() == Some(excluded))
            {
                observed_self = true;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "backup has an active file reader or mapping",
                ));
            }
        }
    }
    if excluded_descriptor.is_some() && !observed_self {
        return Err(io::Error::other(
            "open-reader inventory omitted retained file",
        ));
    }
    Ok(())
}

pub(super) fn move_recovery_debris_into(
    source: &Path,
    destination: &ReclaimDirectory,
) -> io::Result<PathBuf> {
    move_into_with(
        source,
        destination,
        None,
        rename_reclaim_entry,
        sync_reclaim_move_parents,
    )
}

fn move_into_with<R, S>(
    source: &Path,
    destination: &ReclaimDirectory,
    inventoried: Option<&Metadata>,
    mut rename: R,
    mut sync: S,
) -> io::Result<PathBuf>
where
    R: FnMut(&File, &OsStr, &File, &OsStr) -> io::Result<()>,
    S: FnMut(&File, &File) -> io::Result<()>,
{
    let source = absolute_spelling(source)?;
    let name = source
        .file_name()
        .ok_or_else(|| invalid("artifact has no file name"))?;
    let source_parent_path = source
        .parent()
        .ok_or_else(|| invalid("artifact has no parent"))?;
    let source_parent = open_directory(source_parent_path, false)?;
    validate_directory(&destination.path, &destination.file)?;
    let before = rustix::fs::statat(&source_parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    if !matches!(
        FileType::from_raw_mode(before.st_mode),
        FileType::RegularFile | FileType::Directory
    ) {
        return Err(invalid(
            "recovery artifact is not a regular file or real directory",
        ));
    }
    // Retain the object as well as its parent. NONBLOCK prevents a FIFO
    // substitution during the stat/open interval from hanging the operator.
    let source_file = File::from(
        rustix::fs::openat(
            &source_parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let expected = rustix::fs::fstat(&source_file).map_err(io::Error::from)?;
    if !same_object(&before, &expected) || before.st_size != expected.st_size {
        return Err(invalid(
            "recovery artifact changed while acquiring its authority",
        ));
    }
    if let Some(inventoried) = inventoried {
        validate_inventoried_file(&source_file, inventoried)?;
        source_file.sync_all()?;
    }
    for suffix in 0..MAX_RECLAIM_MOVE_ATTEMPTS {
        let mut leaf = name.to_os_string();
        if suffix != 0 {
            leaf.push(format!(".{suffix}"));
        }
        let requested_destination = destination.path.join(&leaf);
        validate_directory(source_parent_path, &source_parent)?;
        validate_directory(&destination.path, &destination.file)?;
        validate_source_entry(&source_parent, name, &expected)?;
        if let Some(inventoried) = inventoried {
            validate_inventoried_file(&source_file, inventoried)?;
        }
        match rename(&source_parent, name, &destination.file, &leaf) {
            Ok(()) => {
                // Always sync the directories that actually participated in
                // rename, even when a subsequent namespace check fails.
                let synced = sync(&source_parent, &destination.file);
                let source_bound = validate_directory(source_parent_path, &source_parent);
                let destination_bound = validate_directory(&destination.path, &destination.file);
                let moved_identity = validate_source_entry(&destination.file, &leaf, &expected);
                let inventory_bound = inventoried.map_or(Ok(()), |metadata| {
                    validate_inventoried_file(&source_file, metadata)
                });
                let mut failures = Vec::new();
                for (phase, result) in [
                    ("parent sync", synced),
                    ("source parent", source_bound),
                    ("destination parent", destination_bound),
                    ("moved identity", moved_identity),
                    ("inventory witness", inventory_bound),
                ] {
                    if let Err(error) = result {
                        failures.push(format!("{phase}: {error}"));
                    }
                }
                if !failures.is_empty() {
                    return Err(io::Error::other(CompletedReclaimMove {
                        requested_destination,
                        failures,
                    }));
                }
                return Ok(requested_destination);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "reclaim collision budget exhausted for {}; no move performed",
            source.display()
        ),
    ))
}

// Existing real-filesystem fault tests use the same implementation with
// explicit operation boundaries; no result or filesystem is simulated.
#[cfg(test)]
pub(super) fn move_recovery_debris(source: &Path, destination: &Path) -> io::Result<PathBuf> {
    move_recovery_debris_with(
        source,
        destination,
        rename_reclaim_entry,
        sync_reclaim_move_parents,
    )
}

#[cfg(test)]
pub(super) fn move_recovery_debris_with<R, S>(
    source: &Path,
    destination: &Path,
    rename: R,
    sync: S,
) -> io::Result<PathBuf>
where
    R: FnMut(&File, &OsStr, &File, &OsStr) -> io::Result<()>,
    S: FnMut(&File, &File) -> io::Result<()>,
{
    let path = absolute_spelling(destination)?;
    let directory = ReclaimDirectory {
        file: open_directory(&path, false)?,
        path,
    };
    move_into_with(source, &directory, None, rename, sync)
}

#[cfg(test)]
pub(super) fn create_private_reclaim_directory(path: &Path, recursive: bool) -> io::Result<()> {
    if recursive {
        open_directory(&absolute_spelling(path)?, true).map(|_| ())
    } else {
        claim_reclaim_directory(path).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_inode_reassembles_short_reads_and_retries_interrupts() {
        struct ShortReads<'a> {
            bytes: &'a [u8],
            reads: usize,
        }
        impl io::Read for ShortReads<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads % 3 == 1 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if self.bytes.is_empty() || output.is_empty() {
                    return Ok(0);
                }
                output[0] = self.bytes[0];
                self.bytes = &self.bytes[1..];
                Ok(1)
            }
        }

        let mut input = ShortReads {
            bytes: b"pos:\t0\nflags:\t0100000\nmnt_id:\t27\nino:\t18446744073709551615\nlock:",
            reads: 0,
        };
        assert_eq!(read_descriptor_inode(&mut input).unwrap(), u64::MAX);
        assert_eq!(input.bytes, b"lock:");
        assert!(input.reads > 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_inode_rejects_missing_malformed_and_partial_records() {
        for bytes in [
            b"pos:\t0\n".as_slice(),
            b"ino:\t",                       // no complete record
            b"ino:\t17",                     // an EOF cannot complete a truncated inode
            b"ino:\t\n",                     // empty identity
            b"ino:\t-1\n",                   // signed identity
            b"ino:\t\xff\n",                 // invalid identity encoding
            b"ino:\t18446744073709551616\n", // overflowing identity
            b"ino:\tbad\nino:\t17\n",        // do not skip a malformed first record
        ] {
            assert!(read_descriptor_inode(bytes).is_err(), "{bytes:?}");
        }
        // As before, unrelated later records do not reinterpret the first
        // authoritative inode emitted by the kernel.
        assert_eq!(
            read_descriptor_inode(b"ino:\t17\nino:\tbad\n".as_slice()).unwrap(),
            17
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_inode_keeps_the_original_scan_bound() {
        let mut complete = vec![b'x'; 4096 - b"\nino:\t17\n".len()];
        complete.extend_from_slice(b"\nino:\t17\n");
        assert_eq!(complete.len(), 4096);
        assert_eq!(read_descriptor_inode(complete.as_slice()).unwrap(), 17);

        // A bound crossing must not turn the prefix of inode 174 into inode 17.
        let mut crossing = vec![b'x'; 4096 - b"\nino:\t17".len()];
        crossing.extend_from_slice(b"\nino:\t174\n");
        let mut input = io::Cursor::new(crossing);
        let error = read_descriptor_inode(&mut input).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(input.position(), 4096);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_inode_preserves_errors_before_a_complete_identity() {
        use std::io::Read as _;

        struct FailedRead(i32);
        impl io::Read for FailedRead {
            fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(self.0))
            }
        }
        for errno in [5, 13, 3] {
            let input = io::Cursor::new(b"pos:\t0\nino:\t1").chain(FailedRead(errno));
            let error = read_descriptor_inode(input).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn empty_directory_reclamation_excludes_live_batches_and_parent_claims() {
        let root = fixture();
        let path = root.join("rotation");
        let batch = ReclaimDirectory::claim(&path).unwrap();
        let inventoried = std::fs::metadata(&path).unwrap();
        assert!(delete_empty_reclaim_directory(&path, &inventoried).is_err());
        assert!(path.is_dir());
        drop(batch);

        let parent = File::open(&root).unwrap();
        fs2::FileExt::try_lock_exclusive(&parent).unwrap();
        assert!(delete_empty_reclaim_directory(&path, &inventoried).is_err());
        let new_batch = root.join("not-yet-claimed");
        assert!(ReclaimDirectory::claim(&new_batch).is_err());
        assert!(!new_batch.exists());
        drop(parent);

        delete_empty_reclaim_directory(&path, &inventoried).unwrap();
        assert!(!path.exists());
        let batch = ReclaimDirectory::claim(&new_batch).unwrap();
        assert!(batch.path().is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn empty_directory_reclamation_preserves_an_ordinary_open_reader() {
        let root = fixture();
        let path = root.join("rotation");
        std::fs::create_dir(&path).unwrap();
        let inventoried = std::fs::metadata(&path).unwrap();
        let reader = File::open(&path).unwrap();
        let error = delete_empty_reclaim_directory(&path, &inventoried).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(reader);
        delete_empty_reclaim_directory(&path, &inventoried).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn empty_directory_reclamation_rejects_replacement_symlinks_and_new_evidence() {
        let root = fixture();
        let path = root.join("rotation");
        std::fs::create_dir(&path).unwrap();
        let inventoried = std::fs::metadata(&path).unwrap();
        let original = root.join("original");
        std::fs::rename(&path, &original).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(delete_empty_reclaim_directory(&path, &inventoried).is_err());
        assert!(path.is_dir());
        assert!(original.is_dir());

        let link = root.join("link");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        assert!(delete_empty_reclaim_directory(&link, &inventoried).is_err());
        let parent_link = root.join("parent-link");
        std::os::unix::fs::symlink(&root, &parent_link).unwrap();
        assert!(
            delete_empty_reclaim_directory(&parent_link.join("original"), &inventoried).is_err()
        );

        let error = delete_empty_reclaim_directory_with(
            &original,
            &inventoried,
            |parent, name| {
                // A recovery writer arrives after the last empty-directory
                // inventory and identity check, immediately before rmdir.
                std::fs::write(original.join("recovery-receipt.json"), b"keep").unwrap();
                rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
            },
            File::sync_all,
        )
        .unwrap_err();
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert_eq!(
            std::fs::read(original.join("recovery-receipt.json")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn empty_directory_reclamation_reports_completed_unconfirmed_removal() {
        let root = fixture();
        let path = root.join("rotation");
        std::fs::create_dir(&path).unwrap();
        let inventoried = std::fs::metadata(&path).unwrap();
        let mut syncs = 0;
        let error = delete_empty_reclaim_directory_with(
            &path,
            &inventoried,
            |parent, name| {
                rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
            },
            |parent| {
                syncs += 1;
                if syncs == 2 {
                    Err(io::Error::other("injected directory sync failure"))
                } else {
                    parent.sync_all()
                }
            },
        )
        .unwrap_err();
        let completed = CompletedReclaimDelete::from_io_error(&error).unwrap();
        assert_eq!(completed.source(), path);
        assert_eq!(completed.bytes(), ReclaimedFileBytes::default());
        assert!(!path.exists());
    }

    #[test]
    fn empty_directory_reclamation_never_follows_a_replaced_parent() {
        let root = fixture();
        let parent_path = root.join("reclaimable");
        std::fs::create_dir(&parent_path).unwrap();
        let path = parent_path.join("rotation");
        std::fs::create_dir(&path).unwrap();
        let inventoried = std::fs::metadata(&path).unwrap();
        let old_parent = root.join("retained-parent");
        let error = delete_empty_reclaim_directory_with(
            &path,
            &inventoried,
            |parent, name| {
                std::fs::rename(&parent_path, &old_parent).unwrap();
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("evidence"), b"keep replacement").unwrap();
                rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
            },
            File::sync_all,
        )
        .unwrap_err();
        assert!(CompletedReclaimDelete::from_io_error(&error).is_some());
        assert!(!old_parent.join("rotation").exists());
        assert_eq!(
            std::fs::read(path.join("evidence")).unwrap(),
            b"keep replacement"
        );
    }

    #[test]
    fn deletion_refuses_replacements_links_and_symlinked_parents() {
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"original").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let retained = root.join("retained");
        std::fs::rename(&source, &retained).unwrap();
        std::fs::write(&source, b"replaced").unwrap();
        assert!(delete_inventoried_file(&source, &inventoried).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"replaced");
        let alias = root.join("alias");
        std::fs::hard_link(&retained, &alias).unwrap();
        assert!(delete_inventoried_file(&alias, &std::fs::metadata(&alias).unwrap()).is_err());
        let symlink = root.join("symlink");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert!(
            delete_inventoried_file(&symlink, &std::fs::symlink_metadata(&symlink).unwrap())
                .is_err()
        );
        let parent_alias = root.join("parent-alias");
        std::os::unix::fs::symlink(&root, &parent_alias).unwrap();
        assert!(
            delete_inventoried_file(
                &parent_alias.join("generation"),
                &std::fs::metadata(&source).unwrap()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(retained).unwrap(), b"original");
        assert_eq!(std::fs::read(source).unwrap(), b"replaced");
    }

    #[cfg(target_os = "linux")]
    fn reader_scan_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_preflight_preserves_same_process_posix_locks() {
        use nix::fcntl::{FcntlArg, fcntl};

        const CHILD_SOURCE: &str = "MCP_AGENT_MAIL_RECLAIM_POSIX_LOCK_PROBE";
        let request = || nix::libc::flock {
            l_type: i16::try_from(nix::libc::F_WRLCK).unwrap(),
            l_whence: i16::try_from(nix::libc::SEEK_SET).unwrap(),
            l_start: 0,
            l_len: 1,
            l_pid: 0,
        };
        if let Some(source) = std::env::var_os(CHILD_SOURCE) {
            let reader = File::open(source).unwrap();
            let mut observed = request();
            fcntl(&reader, FcntlArg::F_GETLK(&mut observed)).unwrap();
            assert_eq!(observed.l_type, i16::try_from(nix::libc::F_RDLCK).unwrap());
            return;
        }
        let _scan_guard = reader_scan_test_guard();

        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"same-process SQLite reader lock").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let reader = File::open(&source).unwrap();
        let mut shared = request();
        shared.l_type = i16::try_from(nix::libc::F_RDLCK).unwrap();
        fcntl(&reader, FcntlArg::F_SETLK(&shared)).unwrap();

        let error = preflight_inventoried_file_readers(&source, &inventoried).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        // F_GETLK ignores locks owned by its own process. A real second
        // process proves that preflight did not close a new candidate handle
        // and silently discard this process's existing SQLite-style lock.
        let full_name = concat!(
            module_path!(),
            "::reader_preflight_preserves_same_process_posix_locks"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", full_name.split_once("::").unwrap().1])
            .env(CHILD_SOURCE, &source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "preflight released a reader lock: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        drop(reader);
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"same-process SQLite reader lock"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_retains_an_open_reader_then_reclaims_after_it_closes() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"retained recovery bytes").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let reader = File::open(&source).unwrap();
        let error = delete_inventoried_file(&source, &inventoried).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert_eq!(std::fs::read(&source).unwrap(), b"retained recovery bytes");
        drop(reader);

        let bytes = delete_inventoried_file(&source, &inventoried).unwrap();
        assert_eq!(bytes.logical_bytes, inventoried.len());
        assert_eq!(bytes.allocated_bytes, inventoried.blocks().checked_mul(512));
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_clean_preflight_never_caches_authority_over_a_later_reader() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"new reader keeps its recovery evidence").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        preflight_inventoried_file_readers(&source, &inventoried).unwrap();

        let reader = File::open(&source).unwrap();
        for result in [
            preflight_inventoried_file_readers(&source, &inventoried),
            delete_inventoried_file(&source, &inventoried).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        }
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"new reader keeps its recovery evidence"
        );
        drop(reader);
        assert_eq!(
            delete_inventoried_file(&source, &inventoried)
                .unwrap()
                .logical_bytes,
            inventoried.len()
        );
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_retains_a_reader_opened_through_an_unlinked_alias() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        let alias = root.join("reader-alias");
        std::fs::write(&source, b"recovery inode retained through another name").unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        let reader = File::open(&alias).unwrap();
        std::fs::remove_file(&alias).unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        assert_eq!(inventoried.nlink(), 1);

        let error = delete_inventoried_file(&source, &inventoried).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert!(source.exists());
        drop(reader);
        assert_eq!(
            delete_inventoried_file(&source, &inventoried)
                .unwrap()
                .logical_bytes,
            inventoried.len()
        );
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_retains_a_mapping_after_its_descriptor_and_alias_are_removed() {
        use std::io::{BufRead as _, Write as _};
        use std::process::Stdio;

        let _scan_guard = reader_scan_test_guard();
        const CONTENT: &[u8] = b"recovery bytes retained only by a memory mapping";
        const READY: &str = "mapping without a descriptor ready";
        const READER: &str = r#"
import mmap
import os
import sys

alias = sys.argv[1]
with open(alias, "rb") as reader:
    identity = os.fstat(reader.fileno())
    mapping = mmap.mmap(reader.fileno(), 0, access=mmap.ACCESS_READ)
# Python before 3.13 duplicates the descriptor inside mmap. Close every copy
# explicitly, then avoid opening files until mapping.close() so its cleanup
# cannot close a subsequently reused descriptor.
for name in os.listdir("/proc/self/fd"):
    descriptor = int(name)
    try:
        observed = os.fstat(descriptor)
    except OSError:
        continue
    if (observed.st_dev, observed.st_ino) == (identity.st_dev, identity.st_ino):
        os.close(descriptor)
os.unlink(alias)
print("mapping without a descriptor ready " + os.readlink("/proc/self"), flush=True)
assert sys.stdin.readline().strip() == "release"
assert mapping[:] == b"recovery bytes retained only by a memory mapping"
mapping.close()
"#;

        let root = fixture();
        let source = root.join("generation");
        let alias = root.join("mapped-reader-alias");
        std::fs::write(&source, CONTENT).unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        let mut child = std::process::Command::new("python3")
            .args(["-u", "-c", READER])
            .arg(&alias)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        output.read_line(&mut ready).unwrap();
        let reader_pid = ready
            .trim()
            .strip_prefix(READY)
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .expect("mapping reader must report its PID in the mounted procfs");
        let inventoried = std::fs::metadata(&source).unwrap();
        let child_descriptors = PathBuf::from(format!("/proc/{reader_pid}/fd"));
        let reader_has_descriptor = std::fs::read_dir(child_descriptors).unwrap().any(|entry| {
            let observed = std::fs::metadata(entry.unwrap().path()).unwrap();
            observed.dev() == inventoried.dev() && observed.ino() == inventoried.ino()
        });

        let preflight = preflight_inventoried_file_readers(&source, &inventoried);
        let deletion = delete_inventoried_file(&source, &inventoried);
        let retained = std::fs::read(&source);
        writeln!(child.stdin.take().unwrap(), "release").unwrap();
        assert!(child.wait().unwrap().success());

        assert!(!reader_has_descriptor, "fixture must hold only a mapping");
        assert!(!alias.exists());
        assert_eq!(inventoried.nlink(), 1);
        for result in [preflight, deletion.map(|_| ())] {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert!(error.to_string().contains("memory mapping"));
            assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        }
        assert_eq!(retained.unwrap(), CONTENT);
        assert_eq!(
            delete_inventoried_file(&source, &inventoried)
                .unwrap()
                .logical_bytes,
            inventoried.len()
        );
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_works_for_an_unprivileged_owner_with_protected_foreign_processes() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        const CHILD: &str = "MCP_AGENT_MAIL_RECLAIM_UNPRIVILEGED_CHILD";
        let own_uid = std::fs::metadata("/proc/self").unwrap().uid();
        if own_uid == 0 && std::env::var_os(CHILD).is_none() {
            let _scan_guard = reader_scan_test_guard();
            let full_name = concat!(
                module_path!(),
                "::deletion_works_for_an_unprivileged_owner_with_protected_foreign_processes"
            );
            let test_name = full_name.split_once("::").unwrap().1;
            // The worker target may have root-private ancestors. Retain this
            // exact executable before dropping privileges; Linux resolves
            // the descriptor before closing it on exec. No directory modes
            // or child privileges change to accommodate the fixture.
            let executable = File::open(std::env::current_exe().unwrap()).unwrap();
            let executable_path = format!("/proc/self/fd/{}", executable.as_raw_fd());
            let output = std::process::Command::new(executable_path) // ubs:ignore — cfg(test), exact retained current ELF descriptor; no external executable selection
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, "1")
                .env("TMPDIR", "/tmp")
                .uid(65_534)
                .gid(65_534)
                .output()
                .unwrap();
            drop(executable);
            assert!(
                output.status.success(),
                "unprivileged deletion failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            return;
        }
        let _scan_guard = reader_scan_test_guard();
        if std::env::var_os(CHILD).is_some() {
            assert_ne!(own_uid, 0);
            let protected = std::fs::read_dir("/proc/1/fd").unwrap_err();
            assert!(matches!(
                protected.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
            ));
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("generation");
        std::fs::write(&source, b"owned backup").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let bytes = delete_inventoried_file(&source, &inventoried).unwrap();
        assert_eq!(bytes.logical_bytes, inventoried.len());
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_retains_a_nondumpable_same_user_reader() {
        use std::io::{BufRead as _, Write as _};
        use std::process::Stdio;

        const CHILD_SOURCE: &str = "MCP_AGENT_MAIL_RECLAIM_NONDUMPABLE_SOURCE";
        const READY: &str = "nondumpable backup reader ready";
        if let Some(source) = std::env::var_os(CHILD_SOURCE) {
            let _reader = File::open(source).unwrap();
            nix::sys::prctl::set_dumpable(false).unwrap();
            println!(
                "{READY} {}",
                std::fs::read_link("/proc/self").unwrap().display()
            );
            std::io::stdout().flush().unwrap();
            let mut release = String::new();
            std::io::stdin().read_line(&mut release).unwrap();
            return;
        }
        let _scan_guard = reader_scan_test_guard();

        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"protected same-user recovery bytes").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let full_name = concat!(
            module_path!(),
            "::deletion_retains_a_nondumpable_same_user_reader"
        );
        let test_name = full_name.split_once("::").unwrap().1;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_SOURCE, &source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let reader_pid = loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "reader exited before readiness"
            );
            if let Some((_, pid)) = line.split_once(READY) {
                break pid
                    .trim()
                    .parse::<u32>()
                    .expect("protected reader must report its PID in the mounted procfs");
            }
            line.clear();
        };
        let child_proc = PathBuf::from(format!("/proc/{reader_pid}"));
        assert_eq!(
            process_reader_uids(&child_proc).unwrap()[1],
            process_reader_uids(Path::new("/proc/self")).unwrap()[1],
        );
        let descriptors_hidden = std::fs::read_dir(child_proc.join("fd"))
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        let caller = read_process_status_summary(Path::new("/proc/self")).unwrap();
        // Inspect this exact protected fixture as well as the public deletion
        // path: another opaque host process must not make this test pass.
        let direct = inspect_process_reader_inventory(
            &child_proc,
            reader_pid,
            &caller,
            &inventoried,
            &source,
            None,
            || Ok(()),
        );
        let result = delete_inventoried_file(&source, &inventoried);
        writeln!(child.stdin.take().unwrap(), "release").unwrap();
        assert!(child.wait().unwrap().success());
        let direct = direct.unwrap_err();
        assert!(matches!(
            direct.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::PermissionDenied
        ));
        if direct.kind() == io::ErrorKind::PermissionDenied {
            assert!(
                direct
                    .to_string()
                    .starts_with(&format!("{}/", child_proc.display())),
                "{direct}"
            );
        }
        let error = result.unwrap_err();
        if descriptors_hidden {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            // The refused process can be another protected one, so this reads
            // the pid back from the message rather than expecting the child's.
            let message = error.to_string();
            let refused_pid = message
                .strip_prefix("/proc/")
                .and_then(|rest| rest.split_once("/fd"))
                .map(|(pid, _)| pid);
            assert!(
                refused_pid.is_some_and(|pid| pid.parse::<u32>().is_ok()),
                "{message}"
            );
        } else {
            // A fully privileged observer can inspect the child, in which
            // case the actual reader (or another protected process) refuses.
            assert!(matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::PermissionDenied
            ));
        }
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"protected same-user recovery bytes"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_refuses_an_active_advisory_reader_lock() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"reader-locked backup").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let reader = File::open(&source).unwrap();
        fs2::FileExt::try_lock_shared(&reader).unwrap();
        let error = delete_inventoried_file(&source, &inventoried).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_permission_failure_retains_bytes_and_does_not_claim_unlink() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"permission-protected backup").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let mut attempted = false;
        let error = delete_inventoried_file_with(
            &source,
            &inventoried,
            |_, _| {
                attempted = true;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
            File::sync_all,
        )
        .unwrap_err();
        assert!(attempted, "fault must occur at the real unlink boundary");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(CompletedReclaimDelete::from_io_error(&error).is_none());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"permission-protected backup"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_reports_post_unlink_sync_failure_without_retrying_a_replacement() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"old completed backup").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        let mut sync_count = 0;
        let error = delete_inventoried_file_with(
            &source,
            &inventoried,
            |parent, name| {
                rustix::fs::unlinkat(parent, name, AtFlags::empty()).map_err(io::Error::from)
            },
            |directory| {
                sync_count += 1;
                if sync_count == 2 {
                    Err(io::Error::other(
                        "injected directory-sync failure after unlink",
                    ))
                } else {
                    directory.sync_all()
                }
            },
        )
        .unwrap_err();
        let completed = CompletedReclaimDelete::from_io_error(&error).unwrap();
        assert_eq!(completed.source(), source);
        assert_eq!(completed.bytes().logical_bytes, inventoried.len());
        assert_eq!(
            completed.bytes().allocated_bytes,
            inventoried.blocks().checked_mul(512)
        );
        assert!(!source.exists(), "unlink took place despite the sync error");

        std::fs::write(&source, b"new evidence at reused pathname").unwrap();
        assert!(delete_inventoried_file(&source, &inventoried).is_err());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"new evidence at reused pathname"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deletion_accounting_distinguishes_sparse_length_from_allocated_blocks() {
        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("sparse-generation");
        let file = File::create(&source).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let inventoried = std::fs::metadata(&source).unwrap();
        let bytes = delete_inventoried_file(&source, &inventoried).unwrap();
        assert_eq!(bytes.logical_bytes, 32 * 1024 * 1024);
        assert_eq!(bytes.allocated_bytes, inventoried.blocks().checked_mul(512));
    }

    #[test]
    fn deletion_refuses_same_length_edits_with_restored_mtime() {
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"original bytes").unwrap();
        let inventoried = std::fs::metadata(&source).unwrap();
        std::fs::write(&source, b"modified bytes").unwrap();
        File::open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(inventoried.modified().unwrap()))
            .unwrap();
        assert!(delete_inventoried_file(&source, &inventoried).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"modified bytes");
    }

    #[test]
    fn symlinked_ancestor_cannot_create_quarantine_outside_the_storage_root() {
        let root = fixture();
        let outside = fixture();
        std::os::unix::fs::symlink(&outside, root.join("doctor")).unwrap();
        assert!(claim_reclaim_directory(&root.join("doctor/reclaimable/run")).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(
            std::fs::symlink_metadata(root.join("doctor"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn claimed_directory_substitution_is_rejected_before_moving_a_source() {
        let root = fixture();
        let requested = root.join("doctor/reclaimable/run");
        let retained = root.join("retained-quarantine");
        let directory = claim_reclaim_directory(&requested).unwrap();
        std::fs::rename(&requested, &retained).unwrap();
        std::fs::create_dir(&requested).unwrap();
        std::fs::write(requested.join("evidence"), b"replacement sentinel").unwrap();
        let source = root.join("evidence");
        std::fs::write(&source, b"source evidence").unwrap();
        assert!(move_recovery_debris_into(&source, &directory).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"source evidence");
        assert_eq!(
            std::fs::read(requested.join("evidence")).unwrap(),
            b"replacement sentinel"
        );
        assert_eq!(std::fs::read_dir(retained).unwrap().count(), 0);
    }

    #[test]
    fn destination_swap_at_rename_cannot_overwrite_the_replacement_namespace() {
        let root = fixture();
        let requested = root.join("quarantine");
        let retained = root.join("retained-quarantine");
        let directory = claim_reclaim_directory(&requested).unwrap();
        let source = root.join("evidence");
        std::fs::write(&source, b"source evidence").unwrap();
        let mut calls = 0;
        let error = move_into_with(
            &source,
            &directory,
            None,
            |from_parent, from, to_parent, to| {
                calls += 1;
                std::fs::rename(&requested, &retained).unwrap();
                std::fs::create_dir(&requested).unwrap();
                std::fs::write(requested.join(to), b"replacement sentinel").unwrap();
                rename_reclaim_entry(from_parent, from, to_parent, to)
            },
            sync_reclaim_move_parents,
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(
            std::fs::read(requested.join("evidence")).unwrap(),
            b"replacement sentinel"
        );
        assert_eq!(
            std::fs::read(retained.join("evidence")).unwrap(),
            b"source evidence"
        );
        assert!(error.to_string().contains("pathname may have changed"));
        assert!(error.to_string().contains("do not retry or roll back"));
        assert_eq!(
            CompletedReclaimMove::from_io_error(&error)
                .unwrap()
                .requested_destination(),
            requested.join("evidence")
        );
    }

    #[test]
    fn source_parent_swap_at_rename_does_not_move_the_replacement_file() {
        let root = fixture();
        let parent = root.join("sources");
        let retained = root.join("retained-sources");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("evidence");
        std::fs::write(&source, b"original evidence").unwrap();
        let directory = claim_reclaim_directory(&root.join("quarantine")).unwrap();
        let error = move_into_with(
            &source,
            &directory,
            None,
            |from_parent, from, to_parent, to| {
                std::fs::rename(&parent, &retained).unwrap();
                std::fs::create_dir(&parent).unwrap();
                std::fs::write(parent.join(from), b"replacement sentinel").unwrap();
                rename_reclaim_entry(from_parent, from, to_parent, to)
            },
            sync_reclaim_move_parents,
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&source).unwrap(), b"replacement sentinel");
        assert_eq!(
            std::fs::read(directory.path.join("evidence")).unwrap(),
            b"original evidence"
        );
        assert!(error.to_string().contains("source parent"));
    }

    #[test]
    fn source_leaf_substitution_between_collision_retries_is_refused() {
        let root = fixture();
        let source = root.join("evidence");
        let original = root.join("original-evidence");
        std::fs::write(&source, b"original").unwrap();
        let directory = claim_reclaim_directory(&root.join("quarantine")).unwrap();
        std::fs::write(directory.path.join("evidence"), b"occupied").unwrap();
        let mut calls = 0;
        let error = move_into_with(
            &source,
            &directory,
            None,
            |from_parent, from, to_parent, to| {
                calls += 1;
                let result = rename_reclaim_entry(from_parent, from, to_parent, to);
                assert_eq!(
                    result.as_ref().unwrap_err().kind(),
                    io::ErrorKind::AlreadyExists
                );
                std::fs::rename(&source, &original).unwrap();
                std::fs::write(&source, b"different replacement").unwrap();
                result
            },
            sync_reclaim_move_parents,
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(std::fs::read(&source).unwrap(), b"different replacement");
        assert_eq!(std::fs::read(original).unwrap(), b"original");
        assert_eq!(
            std::fs::read(directory.path.join("evidence")).unwrap(),
            b"occupied"
        );
        assert!(!directory.path.join("evidence.1").exists());
        assert!(error.to_string().contains("source identity"));
        assert!(CompletedReclaimMove::from_io_error(&error).is_none());
    }

    #[test]
    fn parent_traversal_is_rejected_before_directory_creation() {
        let root = fixture();
        assert!(claim_reclaim_directory(&root.join("absent/../escape")).is_err());
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn private_ancestor_creation_keeps_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let directory = claim_reclaim_directory(&root.join("doctor/reclaimable/run")).unwrap();
        for path in [
            root.join("doctor"),
            root.join("doctor/reclaimable"),
            directory.path,
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn inventoried_file_stage_rejects_same_size_replacement() {
        let root = fixture();
        let source = root.join("backup");
        std::fs::write(&source, b"original").unwrap();
        let inventoried = std::fs::symlink_metadata(&source).unwrap();
        std::fs::rename(&source, root.join("original")).unwrap();
        std::fs::write(&source, b"replaced").unwrap();
        let directory = ReclaimDirectory::claim(&root.join("quarantine")).unwrap();
        let error = directory
            .stage_inventoried_file(&source, &inventoried)
            .unwrap_err();
        assert!(CompletedReclaimMove::from_io_error(&error).is_none());
        assert_eq!(std::fs::read(source).unwrap(), b"replaced");
        assert_eq!(std::fs::read(root.join("original")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn inventoried_file_stage_rejects_changed_content_and_hardlink_alias() {
        for hardlink in [false, true] {
            let root = fixture();
            let source = root.join("backup");
            std::fs::write(&source, b"original").unwrap();
            let inventoried = std::fs::symlink_metadata(&source).unwrap();
            if hardlink {
                std::fs::hard_link(&source, root.join("live-alias")).unwrap();
            } else {
                std::fs::write(&source, b"longer changed content").unwrap();
            }
            let before = std::fs::read(&source).unwrap();
            let directory = ReclaimDirectory::claim(&root.join("quarantine")).unwrap();
            assert!(
                directory
                    .stage_inventoried_file(&source, &inventoried)
                    .is_err()
            );
            assert_eq!(std::fs::read(source).unwrap(), before);
            if hardlink {
                assert_eq!(std::fs::read(root.join("live-alias")).unwrap(), before);
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn inventoried_file_stage_reports_sync_failure_as_a_completed_move() {
        let root = fixture();
        let source = root.join("backup");
        std::fs::write(&source, b"backup contents").unwrap();
        let inventoried = std::fs::symlink_metadata(&source).unwrap();
        let directory = ReclaimDirectory::claim(&root.join("quarantine")).unwrap();
        let error = move_into_with(
            &source,
            &directory,
            Some(&inventoried),
            rename_reclaim_entry,
            |_, _| Err(io::Error::other("injected post-move sync failure")),
        )
        .unwrap_err();
        let completed = CompletedReclaimMove::from_io_error(&error).unwrap();
        assert_eq!(
            completed.requested_destination(),
            directory.path().join("backup")
        );
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(completed.requested_destination()).unwrap(),
            b"backup contents"
        );
    }

    #[test]
    fn inventoried_file_stage_preserves_collision_evidence_and_publishes_exact_file() {
        let root = fixture();
        let source = root.join("backup");
        std::fs::write(&source, b"backup contents").unwrap();
        let inventoried = std::fs::symlink_metadata(&source).unwrap();
        let directory = ReclaimDirectory::claim(&root.join("quarantine")).unwrap();
        std::fs::write(directory.path().join("backup"), b"prior evidence").unwrap();
        let destination = directory
            .stage_inventoried_file(&source, &inventoried)
            .unwrap();
        assert_eq!(destination, directory.path().join("backup.1"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"backup contents");
        assert_eq!(
            std::fs::symlink_metadata(&destination).unwrap().ino(),
            inventoried.ino()
        );
        assert_eq!(
            std::fs::read(directory.path().join("backup")).unwrap(),
            b"prior evidence"
        );
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_task_gone_recognizes_not_found_and_esrch_only() {
        assert!(is_proc_task_gone(&io::Error::from_raw_os_error(2)));
        assert!(is_proc_task_gone(&io::Error::from_raw_os_error(3)));
        assert!(!is_proc_task_gone(&io::Error::from_raw_os_error(13)));
        assert!(!is_proc_task_gone(&io::Error::from_raw_os_error(5)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_entry_error_names_the_entry_and_keeps_the_kind() {
        let denied = io::Error::from_raw_os_error(13);
        let error = proc_entry_error(Path::new("/proc/7/fd/3"), &denied);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            "/proc/7/fd/3: Permission denied (os error 13)"
        );
        assert!(!is_proc_task_gone(&error));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transient_denial_retries_real_descriptors_and_still_refuses_a_reader() {
        for open_reader in [false, true] {
            let root = fixture();
            let source = root.join("generation");
            std::fs::write(&source, b"reader retry fixture").unwrap();
            let identity = std::fs::metadata(&source).unwrap();
            let reader = open_reader.then(|| File::open(&source).unwrap());
            let process = Path::new("/proc").join(std::fs::read_link("/proc/thread-self").unwrap());
            let self_status = read_process_status_summary(&process).unwrap();
            let mut attempts = 0;
            let mut pauses = 0;
            let result = retry_process_reader_scan(
                || {
                    attempts += 1;
                    if attempts == 1 {
                        return Err(io::Error::from_raw_os_error(13));
                    }
                    inspect_process_reader_inventory(
                        &process,
                        std::process::id(),
                        &self_status,
                        &identity,
                        &source,
                        None,
                        || Ok(()),
                    )
                },
                || pauses += 1,
            );
            assert_eq!((attempts, pauses), (2, 1));
            if open_reader {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                assert!(error.to_string().contains("active file reader"));
            } else {
                result.unwrap();
            }
            assert_eq!(std::fs::read(&source).unwrap(), b"reader retry fixture");
            drop(reader);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transient_denial_retries_mapping_inventory_after_empty_descriptors() {
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"mapped retry fixture").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let process = root.join("process");
        std::fs::create_dir_all(process.join("fd")).unwrap();
        // Inject a kernel-shaped mapping record. The existing native mapped
        // reader test separately proves this inventory against real procfs.
        std::fs::write(
            process.join("maps"),
            format!(
                "1000-2000 r--p 00000000 {:02x}:{:02x} {} /removed-alias (deleted)\n",
                nix::sys::stat::major(identity.dev()),
                nix::sys::stat::minor(identity.dev()),
                identity.ino(),
            ),
        )
        .unwrap();
        let self_status = read_process_status_summary(Path::new("/proc/self")).unwrap();
        let mut attempts = 0;
        let error = retry_process_reader_scan(
            || {
                attempts += 1;
                if attempts == 1 {
                    return Err(io::Error::from_raw_os_error(13));
                }
                inspect_process_reader_inventory(
                    &process,
                    4242,
                    &self_status,
                    &identity,
                    &source,
                    None,
                    || Ok(()),
                )
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("active memory mapping"));
        assert_eq!(std::fs::read(source).unwrap(), b"mapped retry fixture");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_retry_is_bounded_and_does_not_retry_other_refusals() {
        for (kind, expected_attempts) in [
            (io::ErrorKind::PermissionDenied, 2),
            (io::ErrorKind::WouldBlock, 1),
            (io::ErrorKind::InvalidInput, 1),
            (io::ErrorKind::Other, 1),
        ] {
            let mut attempts = 0;
            let mut pauses = 0;
            let error = retry_process_reader_scan(
                || {
                    attempts += 1;
                    Err(io::Error::new(kind, "reader evidence unavailable"))
                },
                || pauses += 1,
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(attempts, expected_attempts);
            assert_eq!(pauses, expected_attempts - 1);
        }
        let mut remaining = 1_u32;
        let error = retry_process_reader_scan(
            || {
                remaining = remaining
                    .checked_sub(1)
                    .ok_or_else(|| io::Error::other("inventory bound retained across retry"))?;
                Err(io::Error::from_raw_os_error(13))
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("inventory bound"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_retry_does_not_turn_malformed_owner_evidence_into_absence() {
        let root = fixture();
        std::fs::write(
            root.join("status"),
            b"State:\tS (sleeping)\nFDSize:\t64\nThreads:\t1\nVmSize:\t4096 kB\n",
        )
        .unwrap();
        let caller = read_process_status_summary(Path::new("/proc/self")).unwrap();
        let mut attempts = 0;
        let error = retry_process_reader_scan(
            || {
                attempts += 1;
                if attempts == 1 {
                    return Err(io::Error::from_raw_os_error(13));
                }
                assert!(!reader_inventory_denial_can_be_skipped(
                    &root,
                    &caller,
                    caller.uids[1],
                )?);
                Err(io::Error::from_raw_os_error(13))
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("UID"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_retry_accepts_a_reaped_process_through_its_retained_directory() {
        use std::io::{BufRead as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::process::Stdio;

        let mut child = std::process::Command::new("python3")
            .args([
                "-u",
                "-c",
                "import os,sys; print(os.readlink('/proc/self'),flush=True); sys.stdin.readline()",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pid = String::new();
        let process = std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut pid)
            .and_then(|_| {
                rustix::fs::open(
                    format!("/proc/{}", pid.trim()),
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(io::Error::from)
            });
        // Always release and reap the fixture before asserting observations.
        let release = writeln!(child.stdin.take().unwrap(), "release");
        let exited = child.wait();
        let process = process.unwrap();
        release.unwrap();
        assert!(exited.unwrap().success());
        let pinned = PathBuf::from(format!("/proc/thread-self/fd/{}", process.as_raw_fd()));
        let gone = std::fs::read_to_string(pinned.join("status")).unwrap_err();
        assert!(is_proc_task_gone(&gone), "{gone}");
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"process exit retry fixture").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let caller = read_process_status_summary(Path::new("/proc/self")).unwrap();
        let mut attempts = 0;
        retry_process_reader_scan(
            || {
                attempts += 1;
                if attempts == 1 {
                    return Err(io::Error::from_raw_os_error(13));
                }
                inspect_process_file_readers_once(
                    &pinned,
                    4242,
                    &caller,
                    &identity,
                    &source,
                    None,
                    || Ok(()),
                )
            },
            || {},
        )
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"process exit retry fixture"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreign_reader_requires_disjoint_caller_and_file_owner_uids() {
        let caller = ProcessStatusSummary {
            state: 'S',
            has_vm_size: true,
            fd_size: 64,
            threads: 1,
            uids: [1000; 4],
        };
        for uids in [
            [1000; 4],
            [1000, 0, 0, 0],
            [0, 1000, 0, 0],
            [0, 0, 1000, 0],
            [0, 0, 0, 1000],
            [2000; 4],
        ] {
            let process = ProcessStatusSummary { uids, ..caller };
            assert!(!status_summary_denial_is_foreign(&process, &caller, 2000));
        }
        let foreign = ProcessStatusSummary {
            uids: [3000; 4],
            ..caller
        };
        assert!(status_summary_denial_is_foreign(&foreign, &caller, 2000));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exited_task_requires_all_terminal_reader_evidence() {
        let terminal = ProcessStatusSummary {
            state: 'Z',
            has_vm_size: false,
            fd_size: 0,
            threads: 2,
            uids: [1000; 4],
        };
        // The leader itself is empty; Threads=2 still requires the separate
        // live-task inspection exercised by the real pthread_exit regression.
        assert!(terminal.is_exited_without_files_or_address_space());
        for uncertain in [
            ProcessStatusSummary {
                state: 'S',
                ..terminal
            },
            ProcessStatusSummary {
                has_vm_size: true,
                ..terminal
            },
            ProcessStatusSummary {
                fd_size: 64,
                ..terminal
            },
        ] {
            assert!(!uncertain.is_exited_without_files_or_address_space());
            assert!(!status_summary_denial_is_foreign(
                &uncertain, &terminal, 1000
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_reader_inventory_does_not_follow_a_replaced_process_directory() {
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"pinned process reader fixture").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let alias = root.join("reader-alias");
        std::fs::hard_link(&source, &alias).unwrap();
        let caller = read_process_status_summary(Path::new("/proc/self")).unwrap();
        let process = root.join("4242");
        let original = root.join("original-process");
        let owner = identity.uid();
        let status = format!(
            "State:\tS (sleeping)\nFDSize:\t64\nThreads:\t1\nVmSize:\t4096 kB\nUid:\t{owner}\t{owner}\t{owner}\t{owner}\n"
        );
        let make_empty_process = |path: &Path| -> io::Result<()> {
            std::fs::create_dir_all(path.join("fd"))?;
            std::fs::create_dir(path.join("fdinfo"))?;
            std::fs::write(path.join("maps"), b"")?;
            std::fs::write(path.join("status"), &status)
        };
        make_empty_process(&process).unwrap();
        std::os::unix::fs::symlink(&alias, process.join("fd/7")).unwrap();
        std::fs::write(
            process.join("fdinfo/7"),
            format!("ino:\t{}\n", identity.ino()),
        )
        .unwrap();

        // Replace the numeric pathname after enumeration has charged the
        // original descriptor. A pinned inventory must still inspect that
        // descriptor and its fdinfo through the original directory. The
        // hard-link spelling also requires the device/inode check to work.
        let mut replaced = false;
        let error = inspect_process_reader_inventory(
            &process,
            4242,
            &caller,
            &identity,
            &source,
            None,
            || {
                if !replaced {
                    std::fs::rename(&process, &original)?;
                    make_empty_process(&process)?;
                    replaced = true;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(replaced);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("active file reader"), "{error}");
        assert_eq!(std::fs::read_link(original.join("fd/7")).unwrap(), alias);
        assert!(!process.join("fd/7").exists());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"pinned process reader fixture"
        );

        // The replacement really is reader-free; a failure above must come
        // from the retained original inventory, not a malformed fixture.
        inspect_process_reader_inventory(&process, 4242, &caller, &identity, &source, None, || {
            Ok(())
        })
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_reader_inventory_preserves_original_path_in_descriptor_errors() {
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"descriptor diagnostic fixture").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let caller = read_process_status_summary(Path::new("/proc/self")).unwrap();
        let process = root.join("4242");
        std::fs::create_dir(&process).unwrap();
        std::fs::write(process.join("fd"), b"not a descriptor directory").unwrap();

        let error = inspect_process_reader_inventory(
            &process,
            4242,
            &caller,
            &identity,
            &source,
            None,
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        let detail = error.to_string();
        let expected = format!("{}: ", process.join("fd").display());
        assert!(detail.starts_with(&expected), "{error}");
        assert!(!detail.contains("/proc/thread-self/fd/"), "{error}");
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"descriptor diagnostic fixture"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mapped_reader_scan_failures_name_the_process_and_entry() {
        let root = fixture();
        let retained = std::fs::metadata(&root).unwrap();
        let self_status = read_process_status_summary(Path::new("/proc/self")).unwrap();
        // A directory opens and then fails to read. A self-link fails to open.
        let unreadable = root.join("4242");
        std::fs::create_dir_all(unreadable.join("maps")).unwrap();
        let unopenable = root.join("4243");
        std::fs::create_dir(&unopenable).unwrap();
        std::os::unix::fs::symlink("maps", unopenable.join("maps")).unwrap();
        for process in [unreadable, unopenable] {
            let error =
                ensure_no_mapped_file_readers(&process, 0, &self_status, &retained, || Ok(()))
                    .unwrap_err();
            let expected = format!("{}: ", process.join("maps").display());
            assert!(error.to_string().starts_with(&expected), "{error}");
        }
    }

    #[test]
    fn source_entry_identifies_a_directory_without_its_size() {
        let root = fixture();
        std::fs::create_dir(root.join("bundle")).unwrap();
        std::fs::create_dir(root.join("replacement")).unwrap();
        std::fs::write(root.join("artifact"), b"payload").unwrap();
        let parent = open_directory(&root, false).unwrap();
        let stat =
            |name: &str| rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).unwrap();

        // What xfs does to a small directory on a rename across 2^32.
        let mut resized = stat("bundle");
        resized.st_size += 8;
        validate_source_entry(&parent, OsStr::new("bundle"), &resized).unwrap();

        // Device and inode still decide: a replacement directory is refused.
        let replacement = stat("replacement");
        validate_source_entry(&parent, OsStr::new("bundle"), &replacement).unwrap_err();

        // Anything else keeps its size check.
        let mut shrunk = stat("artifact");
        shrunk.st_size -= 1;
        validate_source_entry(&parent, OsStr::new("artifact"), &shrunk).unwrap_err();
    }

    #[cfg(target_os = "linux")]
    fn superseded_run_unprivileged(test: &str) -> bool {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        if read_process_status_summary(Path::new("/proc/thread-self"))
            .unwrap()
            .uids[1]
            != 0
        {
            return false;
        }
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let capabilities = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
            .unwrap();
        if capabilities & ((1 << 6) | (1 << 7)) != ((1 << 6) | (1 << 7)) {
            // Container UID 0 can have no SETUID/SETGID or DAC capabilities.
            // Such an observer already experiences real permission denials.
            return false;
        }
        // Resolve the already-running ELF before changing credentials. Its
        // build directory may have ancestors that are private to root.
        let executable = File::open("/proc/self/exe").unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output =
            std::process::Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
                .args([
                    "--exact",
                    &format!("{module}::{test}"),
                    "--test-threads=1",
                    "--nocapture",
                ])
                .uid(65_534)
                .gid(65_534)
                .env("TMPDIR", "/tmp")
                .output()
                .unwrap();
        assert!(
            output.status.success(),
            "unprivileged fixture failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        true
    }

    #[cfg(target_os = "linux")]
    fn superseded_process_status(owner: u32) -> String {
        format!(
            "State:\tS (sleeping)\nFDSize:\t64\nThreads:\t1\nVmSize:\t4096 kB\nUid:\t{owner}\t{owner}\t{owner}\t{owner}\n"
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_opacity_requires_readable_identity_for_the_denied_task() {
        use std::os::unix::fs::PermissionsExt as _;

        if superseded_run_unprivileged(
            "superseded_opacity_requires_readable_identity_for_the_denied_task",
        ) {
            return;
        }
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"retained task-identity evidence").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let process = root.join("4242");
        std::fs::create_dir_all(process.join("fd")).unwrap();
        std::fs::write(process.join("maps"), b"").unwrap();
        let status = superseded_process_status(identity.uid());
        std::fs::write(process.join("status"), &status).unwrap();
        std::fs::set_permissions(process.join("fd"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        assert_eq!(
            std::fs::read_dir(process.join("fd")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "the denial must come from the filesystem, not an injected error"
        );
        let caller = read_process_status_summary(Path::new("/proc/thread-self")).unwrap();
        let inspect = || {
            inspect_process_reader_inventory(
                &process,
                4242,
                &caller,
                &identity,
                &source,
                None,
                || Ok(()),
            )
            .unwrap_err()
        };
        let denied = inspect();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            denied
                .get_ref()
                .unwrap()
                .is::<UnobservableReaderInventory>()
        );
        assert!(
            denied
                .to_string()
                .starts_with(&format!("{}/fd:", process.display()))
        );

        std::fs::write(
            process.join("status"),
            b"State:\tS\nFDSize:\t64\nThreads:\t1\n",
        )
        .unwrap();
        let malformed = inspect();
        assert_eq!(malformed.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !malformed
                .get_ref()
                .unwrap()
                .is::<UnobservableReaderInventory>()
        );

        std::fs::write(process.join("status"), &status).unwrap();
        std::fs::set_permissions(
            process.join("status"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let unreadable = inspect();
        std::fs::set_permissions(
            process.join("status"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(unreadable.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !unreadable
                .get_ref()
                .unwrap()
                .is::<UnobservableReaderInventory>()
        );
        assert!(unreadable.to_string().contains("/status:"));
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"retained task-identity evidence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_denied_inventory_pins_reused_process_and_task_paths() {
        use std::os::unix::fs::PermissionsExt as _;

        if superseded_run_unprivileged(
            "superseded_denied_inventory_pins_reused_process_and_task_paths",
        ) {
            return;
        }
        for spelling in ["4242", "4242/task/4243"] {
            let root = fixture();
            let source = root.join("generation");
            std::fs::write(&source, b"retained PID-reuse evidence").unwrap();
            let identity = std::fs::metadata(&source).unwrap();
            let caller = read_process_status_summary(Path::new("/proc/thread-self")).unwrap();
            let process = root.join(spelling);
            let original = root.join("original-task");
            let make_process = |path: &Path, owner| {
                std::fs::create_dir_all(path.join("fd")).unwrap();
                std::fs::create_dir(path.join("fdinfo")).unwrap();
                std::fs::write(path.join("maps"), b"").unwrap();
                std::fs::write(path.join("status"), superseded_process_status(owner)).unwrap();
            };
            make_process(&process, identity.uid());
            std::os::unix::fs::symlink(root.join("unrelated-file"), process.join("fd/7")).unwrap();
            std::fs::write(process.join("fdinfo/7"), b"ino:\t0\n").unwrap();
            std::fs::set_permissions(
                process.join("fdinfo/7"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            let mut replaced = false;
            let error = inspect_process_reader_inventory(
                &process,
                4243,
                &caller,
                &identity,
                &source,
                None,
                || {
                    if !replaced {
                        std::fs::rename(&process, &original)?;
                        // A reused numeric pathname looks unrelated. Both
                        // attempts and the denied task's identity must stay
                        // anchored to the original process directory.
                        make_process(&process, 12_345);
                        replaced = true;
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(replaced);
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.get_ref().unwrap().is::<UnobservableReaderInventory>());
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{}/fd/7:", process.display()))
            );
            assert!(!error.to_string().contains("/proc/thread-self/fd/"));
            inspect_process_reader_inventory(
                &process,
                4243,
                &caller,
                &identity,
                &source,
                None,
                || Ok(()),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(source).unwrap(),
                b"retained PID-reuse evidence"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_denied_descriptors_do_not_hide_readable_mappings() {
        use std::os::unix::fs::PermissionsExt as _;

        if superseded_run_unprivileged(
            "superseded_denied_descriptors_do_not_hide_readable_mappings",
        ) {
            return;
        }
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"visible mapping after denied descriptors").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let process = root.join("4242");
        std::fs::create_dir_all(process.join("fd")).unwrap();
        std::fs::set_permissions(process.join("fd"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        std::fs::write(
            process.join("status"),
            superseded_process_status(identity.uid()),
        )
        .unwrap();
        std::fs::write(
            process.join("maps"),
            format!(
                "1000-2000 r--p 00000000 {:02x}:{:02x} {} /removed-alias (deleted)\n",
                nix::sys::stat::major(identity.dev()),
                nix::sys::stat::minor(identity.dev()),
                identity.ino(),
            ),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_dir(process.join("fd")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let caller = read_process_status_summary(Path::new("/proc/thread-self")).unwrap();
        let error = inspect_process_reader_inventory(
            &process,
            4242,
            &caller,
            &identity,
            &source,
            None,
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("active memory mapping"));
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"visible mapping after denied descriptors"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_denied_descriptor_does_not_hide_a_later_visible_reader() {
        use std::os::unix::fs::PermissionsExt as _;

        if superseded_run_unprivileged(
            "superseded_denied_descriptor_does_not_hide_a_later_visible_reader",
        ) {
            return;
        }
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"visible reader after denied descriptor").unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let process = root.join("4242");
        std::fs::create_dir_all(process.join("fd")).unwrap();
        std::fs::create_dir(process.join("fdinfo")).unwrap();
        std::fs::write(
            process.join("status"),
            superseded_process_status(identity.uid()),
        )
        .unwrap();
        std::fs::write(process.join("maps"), b"").unwrap();
        for name in ["7", "8"] {
            std::os::unix::fs::symlink(root.join("unrelated-file"), process.join("fd").join(name))
                .unwrap();
            std::fs::write(process.join("fdinfo").join(name), b"ino:\t0\n").unwrap();
            std::fs::set_permissions(
                process.join("fdinfo").join(name),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            std::os::unix::fs::symlink(&source, root.join(format!("visible-{name}"))).unwrap();
        }
        let caller = read_process_status_summary(Path::new("/proc/thread-self")).unwrap();
        let mut charged = 0;
        // Exercise a single attempt so a later retry cannot mask stopping at
        // the first denial. On its second entry, whichever order readdir uses,
        // the inventory encounters a genuine visible candidate reference.
        let error = inspect_process_file_readers_once(
            &process,
            4242,
            &caller,
            &identity,
            &source,
            None,
            || {
                charged += 1;
                if charged == 2 {
                    for name in ["7", "8"] {
                        std::fs::rename(
                            root.join(format!("visible-{name}")),
                            process.join("fd").join(name),
                        )?;
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(charged, 2);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("active file reader"));
        assert_eq!(
            std::fs::read(source).unwrap(),
            b"visible reader after denied descriptor"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_opaque_scope_rejects_shared_replaced_and_aliased_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"owned generation").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        validate_opaque_snapshot_scope(&source, &identity, identity.uid()).unwrap();
        assert!(
            validate_opaque_snapshot_scope(&source, &identity, identity.uid().wrapping_add(1))
                .is_err()
        );
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(validate_opaque_snapshot_scope(&source, &identity, identity.uid()).is_err());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        validate_opaque_snapshot_scope(&source, &identity, identity.uid()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let shared_parent = validate_opaque_snapshot_scope(&source, &identity, identity.uid());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(shared_parent.is_err());
        let symbolic = root.join("symbolic");
        std::os::unix::fs::symlink(&source, &symbolic).unwrap();
        assert!(validate_opaque_snapshot_scope(&symbolic, &identity, identity.uid()).is_err());
        std::fs::hard_link(&source, root.join("hard-alias")).unwrap();
        assert!(validate_opaque_snapshot_scope(&source, &identity, identity.uid()).is_err());
        std::fs::rename(&source, root.join("retained-original")).unwrap();
        std::fs::write(&source, b"replacement data").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_opaque_snapshot_scope(&source, &identity, identity.uid()).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"replacement data");
        assert_eq!(
            std::fs::read(root.join("retained-original")).unwrap(),
            b"owned generation"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn superseded_preflight_never_hides_our_own_open_reader() {
        use std::os::unix::fs::PermissionsExt as _;

        let _scan_guard = reader_scan_test_guard();
        let root = fixture();
        let source = root.join("generation");
        std::fs::write(&source, b"own-process reader").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = std::fs::metadata(&source).unwrap();
        let reader = File::open(&source).unwrap();
        let error = preflight_superseded_snapshot_readers(&source, &identity).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("active file reader"));
        drop(reader);
        let evidence = preflight_superseded_snapshot_readers(&source, &identity).unwrap();
        let deleted = delete_superseded_snapshot_file(&source, &identity, evidence).unwrap();
        assert_eq!(deleted.logical_bytes, identity.len());
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn superseded_unlink_preserves_opaque_reader_bytes_and_interrupted_accounting() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Stdio;

        if superseded_run_unprivileged(
            "superseded_unlink_preserves_opaque_reader_bytes_and_interrupted_accounting",
        ) {
            return;
        }
        let _scan_guard = reader_scan_test_guard();
        struct Reader(std::process::Child);
        impl Drop for Reader {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        // A real non-dumpable same-user process. Python mmap may duplicate its
        // input fd, so close every matching descriptor to test mapping-only
        // retention. Every held reference checks its bytes after actual unlink.
        const READER: &str = r"
import ctypes, mmap, os, sys
source, mode = sys.argv[1:]
expected = open(source, 'rb').read()
handle = None
mapping = None
if mode != 'idle':
    handle = os.open(source, os.O_PATH if mode == 'opath' else os.O_RDONLY)
    identity = os.fstat(handle)
    if mode == 'mmap':
        mapping = mmap.mmap(handle, 0, access=mmap.ACCESS_READ)
        for entry in os.listdir('/proc/thread-self/fd'):
            descriptor = int(entry)
            try:
                observed = os.fstat(descriptor)
            except OSError:
                continue
            if (observed.st_dev, observed.st_ino) == (identity.st_dev, identity.st_ino):
                os.close(descriptor)
        handle = None
assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0
print(os.readlink('/proc/self'), flush=True)
assert sys.stdin.readline().strip() == 'release'
if mapping is not None:
    assert mapping[:] == expected
    mapping.close()
elif handle is not None:
    if mode == 'opath':
        readable = os.open('/proc/thread-self/fd/' + str(handle), os.O_RDONLY)
    else:
        readable = handle
    assert os.pread(readable, len(expected), 0) == expected
    if readable != handle:
        os.close(readable)
    os.close(handle)
";
        for mode in ["idle", "fd", "opath", "mmap"] {
            for interrupted in [false, true] {
                let root = fixture();
                let source = root.join("generation");
                std::fs::write(&source, b"opaque readers retain exact backup bytes").unwrap();
                std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
                let identity = std::fs::metadata(&source).unwrap();
                let mut reader = Reader(
                    std::process::Command::new("python3")
                        .args(["-u", "-c", READER])
                        .arg(&source)
                        .arg(mode)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap(),
                );
                let mut pid = String::new();
                std::io::BufReader::new(reader.0.stdout.take().unwrap())
                    .read_line(&mut pid)
                    .unwrap();
                let pid = pid.trim().parse::<u32>().unwrap();
                let process = PathBuf::from(format!("/proc/{pid}"));
                // UID 0 without CAP_SYS_PTRACE can list the root-owned fd
                // directory, but still cannot inspect its targets or maps.
                assert_eq!(
                    std::fs::read(process.join("maps")).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    read_process_status_summary(&process).unwrap().uids,
                    read_process_status_summary(Path::new("/proc/thread-self"))
                        .unwrap()
                        .uids
                );
                if mode == "idle" && !interrupted {
                    use nix::fcntl::{FcntlArg, fcntl};

                    let own_reader = File::open(&source).unwrap();
                    let lock = nix::libc::flock {
                        l_type: i16::try_from(nix::libc::F_RDLCK).unwrap(),
                        l_whence: i16::try_from(nix::libc::SEEK_SET).unwrap(),
                        l_start: 0,
                        l_len: 1,
                        l_pid: 0,
                    };
                    fcntl(&own_reader, FcntlArg::F_SETLK(&lock)).unwrap();
                    let error =
                        preflight_superseded_snapshot_readers(&source, &identity).unwrap_err();
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                    // A separate process must still observe this SQLite-style
                    // lock. Merely seeing our own descriptor does not prove
                    // that preflight avoided opening/closing a second handle.
                    let probe = std::process::Command::new("python3")
                        .args([
                            "-c",
                            "import errno,fcntl,os,sys\nf=os.open(sys.argv[1],os.O_RDWR)\ntry:\n fcntl.lockf(f,fcntl.LOCK_EX|fcntl.LOCK_NB,1)\nexcept OSError as e:\n assert e.errno in (errno.EACCES,errno.EAGAIN)\nelse:\n raise AssertionError('same-process POSIX reader lock was released')\n",
                        ])
                        .arg(&source)
                        .output()
                        .unwrap();
                    assert!(
                        probe.status.success(),
                        "{}",
                        String::from_utf8_lossy(&probe.stderr)
                    );
                    drop(own_reader);
                }
                let strict = preflight_inventoried_file_readers(&source, &identity).unwrap_err();
                assert_eq!(strict.kind(), io::ErrorKind::PermissionDenied);
                let evidence = preflight_superseded_snapshot_readers(&source, &identity).unwrap();
                assert!(evidence.incomplete);
                let mut sync_count = 0;
                let result = delete_inventoried_file_with_evidence(
                    &source,
                    &identity,
                    Some(evidence),
                    |parent, name| {
                        rustix::fs::unlinkat(parent, name, AtFlags::empty())
                            .map_err(io::Error::from)
                    },
                    |directory| {
                        sync_count += 1;
                        if interrupted && sync_count == 2 {
                            Err(io::Error::other("interrupted after actual unlink"))
                        } else {
                            directory.sync_all()
                        }
                    },
                );
                assert!(!source.exists(), "{mode}: the actual unlink must complete");
                writeln!(reader.0.stdin.take().unwrap(), "release").unwrap();
                assert!(
                    reader.0.wait().unwrap().success(),
                    "{mode}: retained reader bytes"
                );
                let bytes = if interrupted {
                    let error = result.unwrap_err();
                    let completed = CompletedReclaimDelete::from_io_error(&error).unwrap();
                    assert_eq!(completed.source(), source);
                    completed.bytes()
                } else {
                    result.unwrap()
                };
                assert_eq!(bytes.logical_bytes, identity.len());
                assert!(bytes.reader_inventory_incomplete);
                assert_eq!(bytes.allocated_bytes, None);
                assert_eq!(sync_count, 2);
                std::fs::write(&source, b"new evidence at the same pathname").unwrap();
                assert!(delete_superseded_snapshot_file(&source, &identity, evidence).is_err());
                assert_eq!(
                    std::fs::read(source).unwrap(),
                    b"new evidence at the same pathname"
                );
            }
        }
    }
}
