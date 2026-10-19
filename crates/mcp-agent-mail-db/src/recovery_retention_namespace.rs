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
/// Dropping it closes the handle; it never removes the directory or evidence.
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
    validate_directory(parent_path, &parent)?;
    rustix::fs::mkdirat(&parent, name, Mode::RWXU).map_err(io::Error::from)?;
    let file = File::from(
        rustix::fs::openat(&parent, name, DIRECTORY_FLAGS, Mode::empty())
            .map_err(io::Error::from)?,
    );
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
    if !same_object(&current, expected) || current.st_size != expected.st_size {
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
    // Also refuse an ordinary observed reader or advisory file lock: unlinking
    // an inode still held open would neither free its blocks nor preserve its
    // pathname for a reader that needs to reopen it.
    fs2::FileExt::try_lock_exclusive(&file)?;
    ensure_no_other_file_readers(&source, &file)?;
    let expected = rustix::fs::fstat(&file)?;
    sync(&parent)?;
    validate_directory(parent_path, &parent)?;
    validate_source_entry(&parent, name, &expected)?;
    validate_delete_inventory(&file, inventoried)?;
    let metadata = file.metadata()?;
    let bytes = ReclaimedFileBytes {
        logical_bytes: metadata.len(),
        allocated_bytes: metadata.blocks().checked_mul(512),
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

/// Check observable readers without opening the candidate at all. Closing a
/// fresh descriptor could release a same-process SQLite reader's POSIX locks,
/// so canonical verification must perform this preflight before its first open.
pub fn preflight_inventoried_file_readers(source: &Path, inventoried: &Metadata) -> io::Result<()> {
    ensure_no_file_readers(source, inventoried, None)
}

fn ensure_no_other_file_readers(source: &Path, retained: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    ensure_no_file_readers(source, &retained.metadata()?, Some(retained.as_raw_fd()))
}

/// External readers do not necessarily participate in snapshot admission.
/// All same-owner processes must be observable; other users' readable handles
/// are checked opportunistically. Linux normally denies inspection of root
/// and other users' processes to an unprivileged mailbox owner, which must not
/// permanently disable cleanup. This is not an atomic exclusion of external
/// opens or invisible other-user readers. `SnapshotAuthority` supplies atomic
/// exclusion for Agent Mail publishers and verified restore readers, while
/// accounting reports unlinked allocation rather than a free-space delta.
#[cfg(target_os = "linux")]
fn ensure_no_file_readers(
    source: &Path,
    retained_identity: &Metadata,
    excluded_descriptor: Option<i32>,
) -> io::Result<()> {
    const MAX_READER_INVENTORY_ENTRIES: usize = 131_072;
    let canonical_source = source.canonicalize()?;
    let own_pid = std::process::id();
    let effective_user = process_reader_uids(Path::new("/proc/self"))?[1];
    let mut remaining = MAX_READER_INVENTORY_ENTRIES;
    let mut charge = || -> io::Result<()> {
        remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("open-reader inventory exceeded its bounded scan"))?;
        Ok(())
    };
    for entry in std::fs::read_dir("/proc")? {
        charge()?;
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let descriptors = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && reader_inventory_denial_is_foreign(&entry.path(), effective_user)? =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        for descriptor in descriptors {
            charge()?;
            let descriptor = descriptor?;
            if pid == own_pid
                && excluded_descriptor.is_some_and(|excluded| {
                    descriptor
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<i32>().ok())
                        == Some(excluded)
                })
            {
                continue;
            }
            let target = match std::fs::read_link(descriptor.path()) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && reader_inventory_denial_is_foreign(&entry.path(), effective_user)? =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let same_inode = if target == canonical_source {
                true
            } else {
                match descriptor_references_inode(&entry.path(), &descriptor, retained_identity) {
                    Ok(same_inode) => same_inode,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error)
                        if error.kind() == io::ErrorKind::PermissionDenied
                            && reader_inventory_denial_is_foreign(
                                &entry.path(),
                                effective_user,
                            )? =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            };
            if same_inode {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("backup has an active file reader in process {pid}"),
                ));
            }
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
    use std::io::Read as _;

    let mut info = Vec::new();
    File::open(process.join("fdinfo").join(descriptor.file_name()))?
        .take(4096)
        .read_to_end(&mut info)?;
    let inode = info
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"ino:"))
        .ok_or_else(|| invalid("open-reader descriptor has no bounded inode record"))?;
    let inode = std::str::from_utf8(inode)
        .map_err(io::Error::other)?
        .trim()
        .parse::<u64>()
        .map_err(io::Error::other)?;
    if inode != retained.ino() {
        return Ok(false);
    }
    let observed = std::fs::metadata(descriptor.path())?;
    Ok(observed.dev() == retained.dev() && observed.ino() == retained.ino())
}

/// A nondumpable process has root-owned proc entries even when its effective
/// user is the mailbox owner. Inspect the kernel's bounded UID record before
/// treating an inaccessible process as another user's observer limitation.
#[cfg(target_os = "linux")]
fn process_reader_uids(process: &Path) -> io::Result<[u32; 4]> {
    use std::io::Read as _;

    const MAX_PROCESS_STATUS_BYTES: usize = 64 * 1024;
    let mut status = Vec::new();
    File::open(process.join("status"))?
        .take(MAX_PROCESS_STATUS_BYTES as u64 + 1)
        .read_to_end(&mut status)?;
    if status.len() > MAX_PROCESS_STATUS_BYTES {
        return Err(invalid(
            "open-reader process status exceeded its bounded scan",
        ));
    }
    let line = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))
        .ok_or_else(|| invalid("open-reader process status has no UID record"))?;
    let mut values = std::str::from_utf8(line)
        .map_err(io::Error::other)?
        .split_whitespace();
    let mut uids = [0; 4];
    for uid in &mut uids {
        *uid = values
            .next()
            .ok_or_else(|| invalid("open-reader process UID record is incomplete"))?
            .parse()
            .map_err(io::Error::other)?;
    }
    if values.next().is_some() {
        return Err(invalid("open-reader process UID record is ambiguous"));
    }
    Ok(uids)
}

#[cfg(target_os = "linux")]
fn reader_inventory_denial_is_foreign(process: &Path, own_uid: u32) -> io::Result<bool> {
    match process_reader_uids(process) {
        Ok(uids) => Ok(!uids.contains(&own_uid)),
        // A process that exited between the denial and status inspection no
        // longer retains a descriptor. Other unavailable evidence refuses.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
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
        tempfile::tempdir().unwrap().keep().canonicalize().unwrap()
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
    fn deletion_retains_a_reader_opened_through_an_unlinked_alias() {
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
    fn deletion_works_for_an_unprivileged_owner_with_protected_foreign_processes() {
        use std::os::unix::process::CommandExt as _;

        const CHILD: &str = "MCP_AGENT_MAIL_RECLAIM_UNPRIVILEGED_CHILD";
        let own_uid = std::fs::metadata("/proc/self").unwrap().uid();
        if own_uid == 0 && std::env::var_os(CHILD).is_none() {
            let full_name = concat!(
                module_path!(),
                "::deletion_works_for_an_unprivileged_owner_with_protected_foreign_processes"
            );
            let test_name = full_name.split_once("::").unwrap().1;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, "1")
                .env("TMPDIR", "/tmp")
                .uid(65_534)
                .gid(65_534)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "unprivileged deletion failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
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
            println!("{READY}");
            std::io::stdout().flush().unwrap();
            let mut release = String::new();
            std::io::stdin().read_line(&mut release).unwrap();
            return;
        }

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
        loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "reader exited before readiness"
            );
            if line.contains(READY) {
                break;
            }
            line.clear();
        }
        let child_proc = PathBuf::from(format!("/proc/{}", child.id()));
        assert_eq!(
            process_reader_uids(&child_proc).unwrap()[1],
            process_reader_uids(Path::new("/proc/self")).unwrap()[1],
        );
        let descriptors_hidden = std::fs::read_dir(child_proc.join("fd"))
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        let result = delete_inventoried_file(&source, &inventoried);
        writeln!(child.stdin.take().unwrap(), "release").unwrap();
        assert!(child.wait().unwrap().success());
        let error = result.unwrap_err();
        if descriptors_hidden {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
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
}
