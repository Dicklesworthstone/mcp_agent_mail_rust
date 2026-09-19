//! Descriptor-bound namespace operations for recovery-debris consolidation.
//!
//! No source is copied or deleted. Each path component is opened without
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
