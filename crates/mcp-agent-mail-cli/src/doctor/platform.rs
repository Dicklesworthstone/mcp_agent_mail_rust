//! Cross-platform primitives for the doctor's mutation, backup, and
//! verification paths.
//!
//! The doctor's hardened guarantees (symlink-swap defense, permission
//! preservation, atomic writes, hash-witnessed backups) were originally
//! written against Unix syscalls (`O_NOFOLLOW`, POSIX mode bits, POSIX
//! symlinks). This module provides the equivalent behavior on Windows
//! using Win32 semantics (reparse-point refusal, the read-only attribute,
//! NTFS symlinks) so the same safety properties hold on both platforms.
//!
//! Unix paths are unchanged from the original inline implementations; the
//! Windows paths use only `std::os::windows` plus Win32 flag constants
//! (mirroring the existing `libc_consts` const-not-a-dep pattern), so no
//! extra dependency is pulled in for the file operations.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

/// Permission bits used in directory-tree hashing and backup preservation.
///
/// Unix: the real POSIX mode (`& 0o7777`).
///
/// Windows: NTFS has no POSIX mode bits, so we synthesize a stable value
/// from the read-only attribute. The hash is only ever compared
/// intra-platform (a backup made on Windows is verified on Windows), so a
/// platform-stable synthetic mode preserves the integrity contract without
/// pretending Windows has Unix permissions.
#[cfg(unix)]
#[must_use]
pub(crate) fn permission_mode(meta: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn permission_mode(meta: &Metadata) -> u32 {
    // Read-only -> 0o444, writable -> 0o644. Stable and deterministic.
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

/// Set permission bits on an already-open file handle (never on a path),
/// so a symlink-swap between persist and chmod cannot redirect the chmod.
///
/// Unix: `Permissions::from_mode(mode)`.
///
/// Windows: map the owner-write bit to the read-only attribute. There is no
/// POSIX mode, but honoring "should this be writable?" is the meaningful
/// part of the contract for the doctor's backup/restore paths.
#[cfg(unix)]
pub(crate) fn set_permission_mode(file: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn set_permission_mode(file: &File, mode: u32) -> io::Result<()> {
    let mut perms = file.metadata()?.permissions();
    // Owner-write bit (0o200) absent => read-only.
    perms.set_readonly(mode & 0o200 == 0);
    file.set_permissions(perms)
}

/// Open a regular file while refusing to traverse a symlink at the final
/// path component (defeating a symlink-swap attacker who replaces the
/// target between detection and open). Returns an error if the opened
/// object is not a regular file.
///
/// Unix: `O_NOFOLLOW | O_NONBLOCK` (the latter also defeats the
/// FIFO-blocks-`open` DoS).
///
/// Windows: `FILE_FLAG_OPEN_REPARSE_POINT` opens the link itself rather than
/// following it; we then reject any object carrying the reparse-point
/// attribute (symlink / junction / mount point), mirroring `O_NOFOLLOW`.
#[cfg(unix)]
pub(crate) fn open_regular_file_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW value as a const so we don't need a libc dep.
    #[cfg(target_os = "linux")]
    const O_NOFOLLOW: i32 = 0o400_000;
    #[cfg(not(target_os = "linux"))]
    const O_NOFOLLOW: i32 = 0x0100;
    const O_NONBLOCK: i32 = 0x0000_0004 | 0x0000_0800; // macOS|Linux superset is harmless here
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    reject_non_regular(&f, path)?;
    Ok(f)
}

#[cfg(not(unix))]
pub(crate) fn open_regular_file_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Win32 flags: open the link itself instead of following it.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    {
        use std::os::windows::fs::MetadataExt;
        let attrs = f.metadata()?.file_attributes();
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a reparse point (symlink/junction); refusing", path.display()),
            ));
        }
    }
    reject_non_regular(&f, path)?;
    Ok(f)
}

fn reject_non_regular(f: &File, path: &Path) -> io::Result<()> {
    if !f.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(())
}

/// A stable byte encoding of an `OsStr` for hashing and exact comparison.
///
/// Unix: the raw bytes (`OsStrExt::as_bytes`).
///
/// Windows: the little-endian UTF-16 code units (`encode_wide`) flattened to
/// bytes — lossless and deterministic, unlike `to_string_lossy`.
#[cfg(unix)]
#[must_use]
pub(crate) fn os_str_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn os_str_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    for unit in s.encode_wide() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Create a filesystem symlink pointing `link` -> `target`.
///
/// Unix: `std::os::unix::fs::symlink`.
///
/// Windows: `symlink_dir`/`symlink_file` depending on the target kind
/// (Windows distinguishes the two; choosing wrong yields a broken link).
/// Requires the process to hold symlink-create privilege (Developer Mode or
/// admin); the error is propagated to the caller as on Unix.
#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    // Resolve the target relative to the link's directory to classify it.
    let resolved = link
        .parent()
        .map_or_else(|| target.to_path_buf(), |p| p.join(target));
    if std::fs::metadata(&resolved).map(|m| m.is_dir()).unwrap_or(false) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Whether `path` is a FIFO/named pipe (Unix) — always `false` on Windows,
/// where the doctor never encounters POSIX FIFOs in a mailbox archive.
#[cfg(unix)]
#[must_use]
pub(crate) fn is_fifo(meta: &Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    meta.file_type().is_fifo()
}

#[cfg(not(unix))]
#[must_use]
pub(crate) fn is_fifo(_meta: &Metadata) -> bool {
    false
}
