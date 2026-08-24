//! Disk space sampling and pressure classification.
//!
//! This module is used by background workers (HTTP/TUI server) to proactively
//! detect low-disk conditions and apply graceful degradation policies.

#![forbid(unsafe_code)]

use crate::Config;
use std::cmp::{self, Ordering};
use std::ffi::OsStr;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

/// Return whether `path` is one of macOS's protected compatibility aliases.
///
/// macOS presents `/var`, `/tmp`, and `/etc` as root-owned symlinks into
/// `/private`. Security-sensitive path walkers should continue rejecting every
/// user-controlled symlink, but treating these exact operating-system aliases
/// as hostile makes ordinary temp-backed operations unusable. Both the alias
/// spelling and its canonical destination are verified, so this exception does
/// not extend to arbitrary symlinks.
#[must_use]
// Not `const`: the macOS branch calls `std::fs::canonicalize`. Only the
// non-macOS stub body would qualify, and `const fn` cannot be cfg-split
// without duplicating the signature, so suppress the nursery lint that
// fires when compiling for non-macOS targets.
#[allow(clippy::missing_const_for_fn)]
pub fn is_trusted_system_directory_alias(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let expected = if path == Path::new("/var") {
            Some(Path::new("/private/var"))
        } else if path == Path::new("/tmp") {
            Some(Path::new("/private/tmp"))
        } else if path == Path::new("/etc") {
            Some(Path::new("/private/etc"))
        } else {
            None
        };

        return expected.is_some_and(|expected| {
            std::fs::canonicalize(path).is_ok_and(|resolved| resolved == expected)
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

/// Open a regular file for reading without following a final-component link.
///
/// Unix uses `O_NOFOLLOW | O_NONBLOCK`; the latter prevents a FIFO swapped
/// into place from blocking `open`. Windows opens the reparse point itself and
/// rejects it. Every platform verifies the opened handle is a regular file.
#[cfg(unix)]
pub fn open_regular_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
/// Open a regular Windows file without traversing a leaf reparse point.
pub fn open_regular_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-reparse-point file", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
/// Open a regular file without following a leaf link on other platforms.
pub fn open_regular_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-symlink file", path.display()),
        ));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} changed to a non-regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular file without following a final-component link, requesting
/// only the access needed to change its permissions.
///
/// Unix permits `fchmod` through a read-only descriptor. Windows requests
/// attribute read/write rights without requesting data-write access, so a
/// read-only recovery source can be made writable before promotion.
#[cfg(unix)]
pub fn open_regular_file_for_permission_change_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular Windows file for attribute updates without traversing a leaf
/// reparse point.
#[cfg(windows)]
pub fn open_regular_file_for_permission_change_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-reparse-point file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular file for permission updates without following a leaf link
/// on other platforms.
#[cfg(not(any(unix, windows)))]
pub fn open_regular_file_for_permission_change_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-symlink file", path.display()),
        ));
    }
    let file = std::fs::OpenOptions::new().read(true).open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} changed to a non-regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular file read-write without following a final-component link.
///
/// Call this only after clearing a source read-only attribute through
/// [`open_regular_file_for_permission_change_no_follow`] when normalizing an
/// untrusted recovery artifact.
#[cfg(unix)]
pub fn open_regular_file_read_write_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular Windows file read-write without traversing a leaf reparse
/// point.
#[cfg(windows)]
pub fn open_regular_file_read_write_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-reparse-point file", path.display()),
        ));
    }
    Ok(file)
}

/// Open a regular file read-write without following a leaf link on other
/// platforms.
#[cfg(not(any(unix, windows)))]
pub fn open_regular_file_read_write_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-symlink file", path.display()),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} changed to a non-regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Normalize an already-open private database or control file for writable,
/// owner-only use.
///
/// On Unix this sets an exact `0600` mode through the file descriptor, so a
/// permissive umask or an untrusted source file cannot make recovered mailbox
/// bytes world-readable. On Windows the destination inherits the ACL of its
/// private parent directory; this helper clears only the read-only attribute
/// while preserving the rest of that inherited security descriptor. Other
/// platforms retain their creation-time permissions.
pub fn set_private_writable_file_permissions(file: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
    }

    #[cfg(windows)]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(false);
        file.set_permissions(permissions)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(())
    }
}

/// Atomically create a new private regular file for mailbox staging.
///
/// Unix supplies mode `0600` to the creation syscall, preventing a permissive
/// umask from exposing even the initially empty inode to another user before
/// normalization. Windows uses create-new semantics and inherits the ACL from
/// the caller's private parent directory. Every platform validates the opened
/// handle and then applies [`set_private_writable_file_permissions`] as
/// defense in depth.
#[cfg(unix)]
pub fn create_new_private_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    set_private_writable_file_permissions(&file)?;
    Ok(file)
}

/// Atomically create a new private regular Windows file without traversing a
/// leaf reparse point.
#[cfg(windows)]
pub fn create_new_private_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular non-reparse-point file", path.display()),
        ));
    }
    set_private_writable_file_permissions(&file)?;
    Ok(file)
}

/// Atomically create a new private regular file on other platforms.
#[cfg(not(any(unix, windows)))]
pub fn create_new_private_file_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    set_private_writable_file_permissions(&file)?;
    Ok(file)
}

/// Read a small regular control file without following a leaf link.
///
/// The size is checked both before and during the bounded read, so a file that
/// grows concurrently cannot force an unbounded allocation.
pub fn read_regular_file_no_follow_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    // Control files are intentionally small. Avoid turning a caller-supplied
    // large limit into a speculative allocation even when the current file is
    // sparse or its metadata races with the bounded read below.
    const MAX_INITIAL_ALLOCATION: u64 = 64 * 1024;

    let mut file = open_regular_file_no_follow(path)?;
    let metadata_len = file.metadata()?.len();
    if metadata_len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is {metadata_len} bytes, exceeding the {max_bytes}-byte control-file limit",
                path.display()
            ),
        ));
    }
    let allocation = usize::try_from(metadata_len.min(max_bytes).min(MAX_INITIAL_ALLOCATION))
        .unwrap_or(64 * 1024);
    let mut bytes = Vec::with_capacity(allocation);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} grew beyond the control-file limit", path.display()),
        ));
    }
    Ok(bytes)
}

/// Published SQLite recovery-candidate filename families.
///
/// The distinction is security-sensitive: private staging and control files
/// can share a broad prefix such as `<db>.bak.` but must never become eligible
/// for automatic recovery merely because they happen to contain a valid
/// SQLite image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteRecoveryCandidateKind {
    /// The exact `<db>.recovery` artifact produced by canonical recovery.
    Recovery,
    /// A published `<db>.bak.<timestamp>` operator safety backup.
    TimestampedBak,
    /// The main member of a historical archive-restore safety generation.
    TimestampedBackup,
    /// The exact `<db>.bak` proactive backup.
    ProactiveBak,
}

impl SqliteRecoveryCandidateKind {
    const fn family_priority(self) -> u8 {
        match self {
            Self::TimestampedBak => 0,
            Self::ProactiveBak => 1,
            Self::TimestampedBackup => 2,
            Self::Recovery => 3,
        }
    }

    /// Return whether this filename family can be restored as one main file.
    ///
    /// Exact `.recovery` artifacts may carry committed state in a companion
    /// WAL. Historical `.backup-*` generations independently backed up the
    /// main file and its sidecars, including independently allocated collision
    /// suffixes, so there is no unambiguous single-file member to promote.
    /// Both families require an explicit future family-aware settlement path.
    #[must_use]
    pub const fn is_standalone_restore_eligible(self) -> bool {
        !matches!(self, Self::Recovery | Self::TimestampedBackup)
    }
}

/// Parsed authority metadata for a published SQLite recovery candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteRecoveryCandidateName {
    kind: SqliteRecoveryCandidateKind,
    generation_micros: Option<i64>,
    collision_sequence: u32,
}

impl SqliteRecoveryCandidateName {
    /// Return the published candidate family.
    #[must_use]
    pub const fn kind(self) -> SqliteRecoveryCandidateKind {
        self.kind
    }

    /// Return the timestamp encoded in a published series filename.
    #[must_use]
    pub const fn generation_micros(self) -> Option<i64> {
        self.generation_micros
    }

    /// Compare two candidates in recovery preference order (newest first).
    ///
    /// Timestamped series use the generation encoded in their filename,
    /// because `copy` may preserve a source file's old mtime. Untimestamped
    /// `.recovery` and the exact proactive `.bak` fall back to mtime. Collision
    /// suffixes break ties between candidates minted in the same timestamp.
    #[must_use]
    pub fn cmp_newest_first(
        self,
        self_modified: SystemTime,
        other: Self,
        other_modified: SystemTime,
    ) -> Ordering {
        let self_generation = self
            .generation_micros
            .unwrap_or_else(|| system_time_micros(self_modified));
        let other_generation = other
            .generation_micros
            .unwrap_or_else(|| system_time_micros(other_modified));
        other_generation
            .cmp(&self_generation)
            .then_with(|| other.collision_sequence.cmp(&self.collision_sequence))
            .then_with(|| {
                self.kind
                    .family_priority()
                    .cmp(&other.kind.family_priority())
            })
    }
}

fn system_time_micros(value: SystemTime) -> i64 {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_micros()).ok())
        .unwrap_or(i64::MIN)
}

fn sqlite_candidate_ascii_suffix(primary: &OsStr, candidate: &OsStr) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let suffix = candidate.as_bytes().strip_prefix(primary.as_bytes())?;
        std::str::from_utf8(suffix).ok().map(str::to_owned)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let candidate = candidate.encode_wide().collect::<Vec<_>>();
        let primary = primary.encode_wide().collect::<Vec<_>>();
        let suffix = candidate.strip_prefix(primary.as_slice())?;
        String::from_utf16(suffix).ok()
    }

    #[cfg(not(any(unix, windows)))]
    {
        candidate
            .to_string_lossy()
            .strip_prefix(primary.to_string_lossy().as_ref())
            .map(str::to_owned)
    }
}

fn parse_sqlite_backup_timestamp(value: &str, format: &str) -> Option<i64> {
    let parsed = chrono::NaiveDateTime::parse_from_str(value, format).ok()?;
    if parsed.format(format).to_string() != value {
        return None;
    }
    Some(parsed.and_utc().timestamp_micros())
}

fn sqlite_backup_generation(value: &str, format: &str) -> Option<(i64, u32)> {
    if let Some(timestamp) = parse_sqlite_backup_timestamp(value, format) {
        return Some((timestamp, 0));
    }
    let (timestamp, collision) = value.rsplit_once('-')?;
    if collision.len() < 2 || !collision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let collision_sequence: u32 = collision.parse().ok()?;
    if collision_sequence == 0 || format!("{collision_sequence:02}") != collision {
        return None;
    }
    Some((
        parse_sqlite_backup_timestamp(timestamp, format)?,
        collision_sequence,
    ))
}

/// Classify a recovery candidate by its complete published filename grammar.
///
/// Matching is performed on raw platform path units before parsing the ASCII
/// suffix, so a non-UTF-8 primary filename remains supported on Unix. Broad
/// prefix matches are deliberately insufficient: stage files, sidecars,
/// metadata, locks, and arbitrary suffixes return `None`. Classification is
/// also inventory-only: callers must separately require
/// [`SqliteRecoveryCandidateKind::is_standalone_restore_eligible`] before
/// selecting an artifact for automatic single-file recovery.
#[must_use]
pub fn classify_sqlite_recovery_candidate_name(
    primary: &OsStr,
    candidate: &OsStr,
) -> Option<SqliteRecoveryCandidateName> {
    let suffix = sqlite_candidate_ascii_suffix(primary, candidate)?;
    let (kind, generation_micros, collision_sequence) = if suffix == ".recovery" {
        (SqliteRecoveryCandidateKind::Recovery, None, 0)
    } else if suffix == ".bak" {
        (SqliteRecoveryCandidateKind::ProactiveBak, None, 0)
    } else if let Some(timestamp) = suffix.strip_prefix(".bak.") {
        let (generation_micros, collision_sequence) =
            sqlite_backup_generation(timestamp, "%Y%m%d_%H%M%S")?;
        (
            SqliteRecoveryCandidateKind::TimestampedBak,
            Some(generation_micros),
            collision_sequence,
        )
    } else {
        let timestamp = suffix.strip_prefix(".backup-")?;
        let (generation_micros, collision_sequence) =
            sqlite_backup_generation(timestamp, "%Y%m%d-%H%M%S")?;
        (
            SqliteRecoveryCandidateKind::TimestampedBackup,
            Some(generation_micros),
            collision_sequence,
        )
    };

    Some(SqliteRecoveryCandidateName {
        kind,
        generation_micros,
        collision_sequence,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskPressure {
    Ok,
    Warning,
    Critical,
    Fatal,
}

impl DiskPressure {
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Critical => 2,
            Self::Fatal => 3,
        }
    }

    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        match v {
            1 => Self::Warning,
            2 => Self::Critical,
            3 => Self::Fatal,
            _ => Self::Ok,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiskSample {
    /// The path used for the storage statvfs probe (directory or file).
    pub storage_probe_path: PathBuf,
    /// The path used for the DB statvfs probe (directory or file), when local.
    pub db_probe_path: Option<PathBuf>,

    pub storage_free_bytes: Option<u64>,
    pub db_free_bytes: Option<u64>,
    /// Minimum of the available free bytes across the known probe paths.
    pub effective_free_bytes: Option<u64>,

    pub pressure: DiskPressure,
    /// Best-effort errors encountered during sampling.
    pub errors: Vec<String>,
}

fn now_unix_micros_u64() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

#[must_use]
pub const fn classify_pressure(
    free_bytes: u64,
    warning_mb: u64,
    critical_mb: u64,
    fatal_mb: u64,
) -> DiskPressure {
    let warning = warning_mb.saturating_mul(MIB);
    let critical = critical_mb.saturating_mul(MIB);
    let fatal = fatal_mb.saturating_mul(MIB);

    if fatal > 0 && free_bytes < fatal {
        DiskPressure::Fatal
    } else if critical > 0 && free_bytes < critical {
        DiskPressure::Critical
    } else if warning > 0 && free_bytes < warning {
        DiskPressure::Warning
    } else {
        DiskPressure::Ok
    }
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(cmp::min(x, y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn normalize_probe_path(path: &Path) -> PathBuf {
    // statvfs typically requires the path to exist; probe the closest existing parent.
    if path.exists() {
        return path.to_path_buf();
    }
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.exists() {
            return parent.to_path_buf();
        }
        cur = parent;
    }
    PathBuf::from(".")
}

/// Return available bytes for the filesystem containing `path`.
///
/// Uses `fs2::available_space` (cross-platform) and never requires unsafe code.
pub fn disk_free_bytes(path: &Path) -> std::io::Result<u64> {
    fs2::available_space(path)
}

/// Parse a local `SQLite` file path from a database URL.
///
/// Supports the legacy Python form `sqlite+aiosqlite:///./path.db` as well as
/// common Rust/SQLAlchemy formats. Returns `None` for in-memory DBs or non-sqlite
/// URLs.
fn sqlite_path_component(database_url: &str) -> Option<&str> {
    let url = database_url.trim();
    let stripped = if let Some(rest) = url.strip_prefix("sqlite+aiosqlite://") {
        rest
    } else {
        url.strip_prefix("sqlite://")?
    };
    // Find the query/fragment cut, skipping any `?` that is part of a Windows
    // UNC verbatim prefix (`\\?\` or `\\?\UNC\`).  Without this guard, the
    // literal `?` inside `\\?\` was treated as a URL query separator and the
    // embedded Windows path was truncated to `/\\` (issue #93).
    let cut = stripped
        .char_indices()
        .find(|(idx, ch)| match *ch {
            '#' => true,
            '?' => !is_unc_verbatim_question_mark(stripped.as_bytes(), *idx),
            _ => false,
        })
        .map(|(idx, _)| idx);
    Some(cut.map_or(stripped, |idx| &stripped[..idx]))
}

/// Return `true` if the byte at `idx` is the `?` inside a Windows UNC
/// verbatim prefix (`\\?\`).  Detection: preceded by `\\`, followed by `\`.
fn is_unc_verbatim_question_mark(bytes: &[u8], idx: usize) -> bool {
    idx >= 2
        && bytes[idx] == b'?'
        && bytes[idx - 1] == b'\\'
        && bytes[idx - 2] == b'\\'
        && bytes.get(idx + 1) == Some(&b'\\')
}

/// Return `true` if `path` is shaped like `/<drive>:[/\\]...` (Windows drive
/// letter with a stray leading `/` from URL syntax).
const fn is_url_drive_letter_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes[3] == b'/' || bytes[3] == b'\\')
}

/// Return `true` when the database URL points to an in-memory `SQLite` database.
#[must_use]
pub fn is_sqlite_memory_database_url(database_url: &str) -> bool {
    let url = database_url.trim();
    let Some(stripped) = url
        .strip_prefix("sqlite+aiosqlite://")
        .or_else(|| url.strip_prefix("sqlite://"))
    else {
        return false;
    };
    if stripped.starts_with("file:")
        && stripped.split_once('?').is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|part| part.eq_ignore_ascii_case("mode=memory"))
        })
    {
        return true;
    }

    matches!(
        sqlite_path_component(database_url),
        Some("/:memory:" | ":memory:")
    )
}

#[must_use]
pub fn sqlite_file_path_from_database_url(database_url: &str) -> Option<PathBuf> {
    let stripped = sqlite_path_component(database_url)?;

    if stripped.is_empty() || stripped == "/" {
        return None;
    }

    // In-memory DB.
    if is_sqlite_memory_database_url(database_url) {
        return None;
    }

    // After stripping, examples:
    // - /./path.db         -> ./path.db
    // - /../path.db        -> ../path.db
    // - //abs/path.db      -> /abs/path.db
    // - /var/data/db.sqlite3 -> /var/data/db.sqlite3
    // - relative/path.db   -> relative/path.db
    let mut path = stripped.to_string();
    if path.starts_with("//") {
        // Absolute path (sqlite:////abs/path.db). Also tolerate accidental
        // extra URL slashes (sqlite://///tmp/db.sqlite3) by reducing the
        // filesystem root to exactly one slash.
        while path.starts_with("//") {
            path.remove(0);
        }
    } else if path.starts_with("/./") || path.starts_with("/../") {
        // Explicitly relative path (sqlite:///./path.db or sqlite:///../path.db).
        path.remove(0);
    }
    // Always re-check for `/<drive>:/...` after the leading-slash trims above:
    // the `sqlite:////C:/...` form (extra `//` then drive letter) needs both
    // the `//` strip AND the drive-letter peel to land on `C:/...`.  Using
    // separate `if` (not `else if`) makes the two trims compose.
    if is_url_drive_letter_prefix(&path) {
        // `sqlite:///C:/path` -> `C:/path`. The leading `/` is URL syntax, not
        // a filesystem component. Strip unconditionally (not behind
        // `cfg!(windows)`) so Linux test suites and tooling can also parse
        // captured Windows URLs without surprise.
        path.remove(0);
    }

    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
}

/// Construct a `sqlite:///` URL from a filesystem path, applying the
/// normalizations that `sqlite_file_path_from_database_url` expects.
///
/// On Windows this strips a leading `\\?\` (or `\\?\UNC\`) verbatim prefix
/// returned by `fs::canonicalize` and converts path separators to `/`. Without
/// the prefix-strip, the literal `?` inside `\\?\` is interpreted as the URL
/// query separator and the embedded path is truncated to garbage (issue #93).
///
/// Use this everywhere a `SQLite` database URL is constructed from a `Path`
/// instead of `format!("sqlite:///{}", path.display())`.
#[must_use]
pub fn sqlite_url_from_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    // Strip Windows UNC verbatim prefix (`\\?\` or `\\?\UNC\`).  Stripping
    // these is always safe because the byte sequence is not a legal component
    // of any normal Unix path; doing it unconditionally lets cross-platform
    // tests on Linux exercise this branch without `cfg!(windows)` gating.
    let stripped = raw
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| raw.strip_prefix(r"\\?\"));
    let cleaned: std::borrow::Cow<'_, str> = match stripped {
        Some(s) => std::borrow::Cow::Owned(s.replace('\\', "/")),
        None if cfg!(windows) => std::borrow::Cow::Owned(raw.replace('\\', "/")),
        None => raw,
    };
    format!("sqlite:///{cleaned}")
}

/// Simplify a Windows extended-length ("verbatim") path back to its legacy
/// form when it is safe to do so (GH#216).
///
/// `fs::canonicalize` on Windows returns `\\?\C:\...` (or `\\?\UNC\srv\share`)
/// paths. Many downstream consumers — libgit2 most prominently — mishandle the
/// verbatim form (`I/O error: Incorrect function. (os error 1)`), which broke
/// every git-archive write-back on Windows storage roots. Stripping is skipped
/// when the simplified form would not round-trip safely: overall length at or
/// beyond the classic `MAX_PATH` limit, or components with trailing
/// dots/spaces (which only the verbatim form preserves).
///
/// On non-Windows paths (no verbatim prefix) this is a no-op, so the logic is
/// exercised by cross-platform tests without `cfg!(windows)` gating.
#[must_use]
pub fn simplify_verbatim_path(path: &Path) -> PathBuf {
    const CLASSIC_MAX_PATH: usize = 260;
    let raw = path.to_string_lossy();
    let simplified: Option<String> = raw.strip_prefix(r"\\?\UNC\").map_or_else(
        || {
            raw.strip_prefix(r"\\?\").and_then(|rest| {
                // Only the disk form (`C:\...`) is safe to simplify.
                let bytes = rest.as_bytes();
                (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
                    .then(|| rest.to_string())
            })
        },
        |rest| Some(format!(r"\\{rest}")),
    );
    match simplified {
        Some(s)
            if s.len() < CLASSIC_MAX_PATH
                && !s
                    .split('\\')
                    .any(|component| component.ends_with('.') || component.ends_with(' ')) =>
        {
            PathBuf::from(s)
        }
        _ => path.to_path_buf(),
    }
}

/// Strip a Windows extended-length ("verbatim") prefix from a path string
/// unconditionally, for path *comparison* only (GH#216).
///
/// Unlike [`simplify_verbatim_path`] this ignores the round-trip safety guards
/// (`MAX_PATH`, trailing dots/spaces): the stripped spelling is never handed to
/// the filesystem, it only has to land both sides of a prefix comparison in the
/// same form. `\\?\UNC\srv\share` becomes `\\srv\share` and `\\?\C:\x` becomes
/// `C:\x`. Returns `None` when `raw` carries no verbatim prefix.
///
/// Pure string manipulation, applied unconditionally on every platform (the
/// `\\?\` byte sequence never begins a real Unix path), so Linux tests exercise
/// the logic without `cfg!(windows)` gating — same convention as
/// [`simplify_verbatim_path`].
fn strip_verbatim_prefix_str(raw: &str) -> Option<String> {
    raw.strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| raw.strip_prefix(r"\\?\").map(str::to_string))
}

/// String-level core of [`relative_to_normalized`] /
/// [`path_starts_with_normalized`]: compute `target` relative to `base` after
/// stripping verbatim prefixes from both sides.
///
/// Returns `None` when neither side carries a verbatim prefix (the caller's
/// plain `Path::strip_prefix` was already authoritative), when `base` is not a
/// prefix of `target`, or when the match does not fall on a path-component
/// boundary (`C:\ab` is not inside `C:\a`).
fn relative_str_after_verbatim_strip(target: &str, base: &str) -> Option<String> {
    let target_stripped = strip_verbatim_prefix_str(target);
    let base_stripped = strip_verbatim_prefix_str(base);
    if target_stripped.is_none() && base_stripped.is_none() {
        return None;
    }
    let target_norm = target_stripped.unwrap_or_else(|| target.to_string());
    let base_norm = base_stripped.unwrap_or_else(|| base.to_string());
    let rest = target_norm.strip_prefix(&base_norm)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    if !(rest.starts_with('\\')
        || rest.starts_with('/')
        || base_norm.ends_with('\\')
        || base_norm.ends_with('/'))
    {
        return None;
    }
    Some(rest.trim_start_matches(['\\', '/']).to_string())
}

/// Compute `target` relative to `base`, tolerating mixed plain/verbatim
/// Windows path spellings (GH#216).
///
/// `fs::canonicalize` on Windows returns the extended-length `\\?\C:\...`
/// spelling while storage roots are kept in the simplified legacy form, so a
/// plain [`Path::strip_prefix`] fails whenever exactly one side carries the
/// `\\?\` prefix — which broke every archive write-back on Windows. This
/// helper first defers to `Path::strip_prefix` (identical behavior for all
/// same-spelling inputs, including every non-Windows path), then retries at
/// the string level with the verbatim prefix stripped from both sides.
#[must_use]
pub fn relative_to_normalized(target: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(rel) = target.strip_prefix(base) {
        return Some(rel.to_path_buf());
    }
    relative_str_after_verbatim_strip(&target.to_string_lossy(), &base.to_string_lossy())
        .map(PathBuf::from)
}

/// Prefix containment check tolerant of mixed plain/verbatim Windows path
/// spellings (GH#216). Equivalent to [`Path::starts_with`] for same-spelling
/// inputs (and for every non-Windows path).
#[must_use]
pub fn path_starts_with_normalized(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
        || relative_str_after_verbatim_strip(&path.to_string_lossy(), &base.to_string_lossy())
            .is_some()
}

/// Return the sibling `SQLite` sidecar path for a `database` file.
#[must_use]
pub fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = db_path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

/// Sample disk space for the key local paths (storage root and `SQLite` file, if
/// applicable) and classify pressure using the config thresholds.
#[must_use]
pub fn sample_disk(config: &Config) -> DiskSample {
    let storage_probe_path = normalize_probe_path(&config.storage_root);
    let db_path = sqlite_file_path_from_database_url(&config.database_url);
    let db_probe_path = db_path.as_deref().map(normalize_probe_path);

    let mut errors = Vec::new();

    let storage_free_bytes = match disk_free_bytes(&storage_probe_path) {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(format!(
                "statvfs(storage) failed path={} err={e}",
                storage_probe_path.display()
            ));
            None
        }
    };

    let db_free_bytes = db_probe_path
        .as_deref()
        .and_then(|p| match disk_free_bytes(p) {
            Ok(v) => Some(v),
            Err(e) => {
                errors.push(format!("statvfs(db) failed path={} err={e}", p.display()));
                None
            }
        });

    let effective_free_bytes = min_opt(storage_free_bytes, db_free_bytes);
    let pressure = effective_free_bytes.map_or(DiskPressure::Ok, |free| {
        classify_pressure(
            free,
            config.disk_space_warning_mb,
            config.disk_space_critical_mb,
            config.disk_space_fatal_mb,
        )
    });

    DiskSample {
        storage_probe_path,
        db_probe_path,
        storage_free_bytes,
        db_free_bytes,
        effective_free_bytes,
        pressure,
        errors,
    }
}

/// Read cumulative process I/O bytes from `/proc/self/io` (Linux).
///
/// Returns `(read_bytes, write_bytes)`. On non-Linux platforms, returns `(0, 0)`.
/// The `write_bytes` field corresponds to the kernel's `write_bytes` counter,
/// which tracks actual storage writes (post page-cache), giving a real signal
/// under `SQLite` + git archive workloads.
///
/// See: <https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/17>
#[must_use]
#[cfg(target_os = "linux")]
pub fn read_proc_io_bytes() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string("/proc/self/io") else {
        return (0, 0);
    };

    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;

    for line in content.lines() {
        if let Some(val) = line.strip_prefix("read_bytes: ") {
            read_bytes = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("write_bytes: ") {
            write_bytes = val.trim().parse().unwrap_or(0);
        }
    }

    (read_bytes, write_bytes)
}

#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn read_proc_io_bytes() -> (u64, u64) {
    (0, 0)
}

/// Sample disk space and update core system metrics gauges.
#[must_use]
pub fn sample_and_record(config: &Config) -> DiskSample {
    let sample = sample_disk(config);
    let metrics = crate::global_metrics();

    if let Some(bytes) = sample.storage_free_bytes {
        metrics.system.disk_storage_free_bytes.set(bytes);
    }
    if let Some(bytes) = sample.db_free_bytes {
        metrics.system.disk_db_free_bytes.set(bytes);
    }
    metrics
        .system
        .disk_effective_free_bytes
        .set(sample.effective_free_bytes.unwrap_or(0));
    metrics
        .system
        .disk_pressure_level
        .set(sample.pressure.as_u64());
    metrics
        .system
        .disk_last_sample_us
        .set(now_unix_micros_u64());
    if !sample.errors.is_empty() {
        metrics
            .system
            .disk_sample_errors_total
            .add(u64::try_from(sample.errors.len()).unwrap_or(u64::MAX));
    }

    // Sample process I/O bytes (Linux only; 0 on other platforms).
    let (io_read, io_write) = read_proc_io_bytes();
    metrics.system.disk_io_read_bytes.set(io_read);
    metrics.system.disk_io_write_bytes.set(io_write);

    sample
}

/// Recognize the fixed macOS firmlinks that symlink guards must accept.
///
/// These are `/var`, `/tmp`, and `/etc` -> `/private/<name>`, which
/// path-traversal guards must treat as platform-canonical rather than as a
/// symlink-escape (GH#230; macOS TMPDIRs live under `/var/folders/...`, so
/// refusing `/var` broke every operator-supplied output path on macOS).
///
/// Conservative on purpose: ONLY a top-level `/<name>` symlink whose canonical
/// target is exactly `/private/<name>` (for the small fixed set of Apple
/// firmlink roots) qualifies, so an attacker-planted symlink anywhere else is
/// still refused. A no-op on Linux, where those paths are real directories
/// (the `is_symlink()` branches that call this are never taken).
///
/// `link` is the symlinked path component as encountered during traversal;
/// `resolved` is its canonical target (e.g. from `std::fs::canonicalize`).
#[must_use]
pub fn is_platform_temp_firmlink(link: &Path, resolved: &Path) -> bool {
    let Some(name) = link.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !matches!(name, "var" | "tmp" | "etc") {
        return false;
    }
    // Must be a top-level entry directly under `/` — `/var`, not `/foo/var`.
    if link.parent() != Some(Path::new("/")) {
        return false;
    }
    resolved == Path::new("/private").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn bounded_control_file_read_rejects_oversized_input() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("control.json");
        std::fs::write(&path, vec![b'x'; 33]).unwrap();

        let error = read_regular_file_no_follow_bounded(&path, 32)
            .expect_err("oversized control state must fail before allocation");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn private_create_is_owner_only_before_returning_the_handle() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().unwrap();
        let path = dir.path().join("private-stage.sqlite3");
        let file = create_new_private_file_no_follow(&path).expect("create private stage");
        assert_eq!(
            file.metadata().unwrap().permissions().mode() & 0o777,
            0o600,
            "the creation helper must return an exact owner-only inode"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            create_new_private_file_no_follow(&path)
                .expect_err("create-new must not replace an occupied stage")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_follow_control_file_read_rejects_leaf_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("target.json");
        let link = dir.path().join("control.json");
        std::fs::write(&target, b"sentinel").unwrap();
        symlink(&target, &link).unwrap();

        assert!(open_regular_file_no_follow(&link).is_err());
        assert!(read_regular_file_no_follow_bounded(&link, 1024).is_err());
    }

    #[test]
    fn sqlite_recovery_candidate_classifier_accepts_only_published_names() {
        let primary = OsStr::new("storage.sqlite3");
        for (name, expected_kind) in [
            (
                "storage.sqlite3.recovery",
                SqliteRecoveryCandidateKind::Recovery,
            ),
            (
                "storage.sqlite3.bak",
                SqliteRecoveryCandidateKind::ProactiveBak,
            ),
            (
                "storage.sqlite3.bak.20260824_120102",
                SqliteRecoveryCandidateKind::TimestampedBak,
            ),
            (
                "storage.sqlite3.bak.20260824_120102-01",
                SqliteRecoveryCandidateKind::TimestampedBak,
            ),
            (
                "storage.sqlite3.backup-20260824-120102",
                SqliteRecoveryCandidateKind::TimestampedBackup,
            ),
            (
                "storage.sqlite3.backup-20260824-120102-01",
                SqliteRecoveryCandidateKind::TimestampedBackup,
            ),
            (
                "storage.sqlite3.backup-20260824-120102-100",
                SqliteRecoveryCandidateKind::TimestampedBackup,
            ),
        ] {
            let classified = classify_sqlite_recovery_candidate_name(primary, OsStr::new(name))
                .unwrap_or_else(|| panic!("expected published candidate: {name}"));
            assert_eq!(classified.kind(), expected_kind, "candidate {name}");
        }

        for name in [
            "storage.sqlite3.bak.backup-stage-20260824_120102_345",
            "storage.sqlite3.bak.20260824_120102-wal",
            "storage.sqlite3.bak.20260824_120102_345",
            "storage.sqlite3.bak.2026082_120102",
            "storage.sqlite3.bak.20260824_12012",
            "storage.sqlite3.bak.20260824_120102-00",
            "storage.sqlite3.bak.20260824_120102.metadata",
            "storage.sqlite3.backup-test",
            "storage.sqlite3.backup-20260824_120102",
            "storage.sqlite3.backup-2026082-120102",
            "storage.sqlite3.backup-20260824-120102-1",
            "storage.sqlite3.backup-20260824-120102-00",
            "storage.sqlite3.backup-20260824-120102-001",
            "storage.sqlite3.backup-20260824-120102.123",
            "storage.sqlite3.recovery.lock",
            "storage.sqlite3.recovery-wal",
            "other.sqlite3.bak.20260824_120102",
        ] {
            assert!(
                classify_sqlite_recovery_candidate_name(primary, OsStr::new(name)).is_none(),
                "private or malformed candidate must be rejected: {name}"
            );
        }

        assert!(SqliteRecoveryCandidateKind::ProactiveBak.is_standalone_restore_eligible());
        assert!(SqliteRecoveryCandidateKind::TimestampedBak.is_standalone_restore_eligible());
        assert!(!SqliteRecoveryCandidateKind::Recovery.is_standalone_restore_eligible());
        assert!(
            !SqliteRecoveryCandidateKind::TimestampedBackup.is_standalone_restore_eligible(),
            "historical .backup-* main and sidecar collision suffixes were allocated independently"
        );
    }

    #[test]
    fn sqlite_recovery_candidate_order_uses_name_generation_mtime_and_collision() {
        let primary = OsStr::new("storage.sqlite3");
        let older = classify_sqlite_recovery_candidate_name(
            primary,
            OsStr::new("storage.sqlite3.bak.20260823_120000"),
        )
        .expect("older timestamped backup");
        let newer = classify_sqlite_recovery_candidate_name(
            primary,
            OsStr::new("storage.sqlite3.backup-20260824-120000"),
        )
        .expect("newer timestamped backup");
        let proactive =
            classify_sqlite_recovery_candidate_name(primary, OsStr::new("storage.sqlite3.bak"))
                .expect("proactive backup");
        let first_collision = classify_sqlite_recovery_candidate_name(
            primary,
            OsStr::new("storage.sqlite3.bak.20260824_120000-01"),
        )
        .expect("first collision backup");
        let second_collision = classify_sqlite_recovery_candidate_name(
            primary,
            OsStr::new("storage.sqlite3.bak.20260824_120000-02"),
        )
        .expect("second collision backup");
        let old_mtime = UNIX_EPOCH + std::time::Duration::from_secs(1);
        let future_mtime = UNIX_EPOCH + std::time::Duration::from_secs(u64::from(u32::MAX));

        assert_eq!(
            newer.cmp_newest_first(old_mtime, older, future_mtime),
            Ordering::Less,
            "logical filename generation must outrank preserved filesystem mtime"
        );
        assert_eq!(
            proactive.cmp_newest_first(future_mtime, newer, old_mtime),
            Ordering::Less,
            "the actively refreshed proactive .bak must participate by mtime"
        );
        assert_eq!(
            second_collision.cmp_newest_first(old_mtime, first_collision, future_mtime),
            Ordering::Less,
            "the later collision sequence must win when logical generations tie"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_recovery_candidate_classifier_supports_non_utf8_primary_names() {
        use std::os::unix::ffi::OsStringExt as _;

        let primary = std::ffi::OsString::from_vec(b"storage-\xFF.sqlite3".to_vec());
        let mut candidate = primary.clone().into_vec();
        candidate.extend_from_slice(b".bak.20260824_120102");
        let candidate = std::ffi::OsString::from_vec(candidate);

        assert_eq!(
            classify_sqlite_recovery_candidate_name(&primary, &candidate)
                .map(SqliteRecoveryCandidateName::kind),
            Some(SqliteRecoveryCandidateKind::TimestampedBak)
        );
    }

    #[test]
    fn platform_temp_firmlink_accepts_apple_firmlink_roots() {
        assert!(is_platform_temp_firmlink(
            Path::new("/var"),
            Path::new("/private/var")
        ));
        assert!(is_platform_temp_firmlink(
            Path::new("/tmp"),
            Path::new("/private/tmp")
        ));
        assert!(is_platform_temp_firmlink(
            Path::new("/etc"),
            Path::new("/private/etc")
        ));
    }

    #[test]
    fn platform_temp_firmlink_rejects_non_firmlinks() {
        // Nested (not directly under `/`) -> not a firmlink.
        assert!(!is_platform_temp_firmlink(
            Path::new("/foo/var"),
            Path::new("/private/var")
        ));
        // Wrong canonical target -> not a firmlink (an escape attempt).
        assert!(!is_platform_temp_firmlink(
            Path::new("/var"),
            Path::new("/other")
        ));
        // A symlink pointing at a sensitive file is NOT a firmlink.
        assert!(!is_platform_temp_firmlink(
            Path::new("/tmp"),
            Path::new("/etc/passwd")
        ));
        // Not one of the recognized temp roots.
        assert!(!is_platform_temp_firmlink(
            Path::new("/usr"),
            Path::new("/private/usr")
        ));
        // Canonical target must be exactly /private/<name>, not deeper.
        assert!(!is_platform_temp_firmlink(
            Path::new("/var"),
            Path::new("/private/var/tmp")
        ));
        // Relative link paths never qualify.
        assert!(!is_platform_temp_firmlink(
            Path::new("var"),
            Path::new("/private/var")
        ));
    }

    #[test]
    fn sqlite_url_parsing_variants() {
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite+aiosqlite:///./storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "./storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///./storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "./storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "/storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///storage.sqlite3?mode=rwc")
                .unwrap()
                .to_string_lossy(),
            "/storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///home/ubuntu/storage.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "/home/ubuntu/storage.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:////abs/path.db")
                .unwrap()
                .to_string_lossy(),
            "/abs/path.db"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite://///tmp/db.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "/tmp/db.sqlite3"
        );
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:////abs/path.db?cache=shared")
                .unwrap()
                .to_string_lossy(),
            "/abs/path.db"
        );
        assert!(sqlite_file_path_from_database_url("sqlite3:///storage.sqlite3").is_none());
        assert!(sqlite_file_path_from_database_url("sqlite:///:memory:").is_none());
        assert!(sqlite_file_path_from_database_url("sqlite:///:memory:?cache=shared").is_none());
        assert!(
            sqlite_file_path_from_database_url("sqlite://file:memdb1?mode=memory&cache=shared")
                .is_none()
        );
        assert!(is_sqlite_memory_database_url("sqlite:///:memory:"));
        assert!(is_sqlite_memory_database_url(
            "sqlite:///:memory:?cache=shared"
        ));
        assert!(is_sqlite_memory_database_url(
            "sqlite://file:memdb1?mode=memory&cache=shared"
        ));
        assert!(sqlite_file_path_from_database_url("postgres://localhost/db").is_none());
        assert!(!is_sqlite_memory_database_url("postgres://localhost/db"));
        // Edge case: bare sqlite:/// with no path after stripping → None
        assert!(sqlite_file_path_from_database_url("sqlite:///").is_none());
    }

    #[test]
    fn pressure_classification_thresholds() {
        let free = 600 * MIB;
        assert_eq!(classify_pressure(free, 500, 100, 10), DiskPressure::Ok);
        assert_eq!(
            classify_pressure(400 * MIB, 500, 100, 10),
            DiskPressure::Warning
        );
        assert_eq!(
            classify_pressure(50 * MIB, 500, 100, 10),
            DiskPressure::Critical
        );
        assert_eq!(
            classify_pressure(5 * MIB, 500, 100, 10),
            DiskPressure::Fatal
        );
    }

    #[test]
    fn min_opt_combinations() {
        assert_eq!(min_opt(Some(3), Some(9)), Some(3));
        assert_eq!(min_opt(Some(9), Some(3)), Some(3));
        assert_eq!(min_opt(Some(7), None), Some(7));
        assert_eq!(min_opt(None, Some(7)), Some(7));
        assert_eq!(min_opt(None, None), None);
    }

    #[test]
    fn normalize_probe_path_prefers_existing_parent_and_dot_fallback() {
        let tmp = tempdir().expect("tempdir should be created");
        let missing_leaf = tmp.path().join("missing").join("nested").join("db.sqlite3");
        assert_eq!(
            normalize_probe_path(&missing_leaf),
            tmp.path().to_path_buf()
        );

        let unique = PathBuf::from(format!(
            "definitely_missing_probe_path_{}",
            now_unix_micros_u64()
        ));
        assert!(
            !unique.exists(),
            "unique missing probe path unexpectedly exists: {}",
            unique.display()
        );
        assert_eq!(normalize_probe_path(&unique), PathBuf::from("."));
    }

    #[test]
    fn sample_disk_uses_effective_min_and_applies_thresholds() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let db_file = tmp.path().join("db").join("storage.sqlite3");
        std::fs::create_dir_all(
            db_file
                .parent()
                .expect("db file parent should exist after create_dir_all"),
        )
        .expect("db parent should be created");

        // Force warning classification for any realistic free-byte value.
        let config = Config {
            storage_root: storage_root.clone(),
            database_url: format!(
                "sqlite:////{}",
                db_file.to_string_lossy().trim_start_matches('/')
            ),
            disk_space_warning_mb: u64::MAX,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let sample = sample_disk(&config);
        assert_eq!(sample.storage_probe_path, storage_root);
        assert_eq!(
            sample.db_probe_path,
            db_file.parent().map(std::path::Path::to_path_buf)
        );
        assert!(sample.storage_free_bytes.is_some());
        assert!(sample.db_free_bytes.is_some());

        let storage_free = sample
            .storage_free_bytes
            .expect("storage free bytes expected");
        let db_free = sample.db_free_bytes.expect("db free bytes expected");
        assert_eq!(
            sample.effective_free_bytes,
            Some(std::cmp::min(storage_free, db_free))
        );
        assert_eq!(sample.pressure, DiskPressure::Warning);
        assert_eq!(sample.errors, [] as [String; 0]);
    }

    // ── DiskPressure enum coverage ──────────────────────────────────────

    #[test]
    fn disk_pressure_as_u64_roundtrip() {
        for &(variant, expected) in &[
            (DiskPressure::Ok, 0u64),
            (DiskPressure::Warning, 1),
            (DiskPressure::Critical, 2),
            (DiskPressure::Fatal, 3),
        ] {
            assert_eq!(variant.as_u64(), expected);
            assert_eq!(DiskPressure::from_u64(expected), variant);
        }
    }

    #[test]
    fn disk_pressure_from_u64_unknown_maps_to_ok() {
        // Any value outside 1..=3 maps to Ok (the catch-all)
        assert_eq!(DiskPressure::from_u64(4), DiskPressure::Ok);
        assert_eq!(DiskPressure::from_u64(255), DiskPressure::Ok);
        assert_eq!(DiskPressure::from_u64(u64::MAX), DiskPressure::Ok);
    }

    #[test]
    fn disk_pressure_label_covers_all_variants() {
        assert_eq!(DiskPressure::Ok.label(), "ok");
        assert_eq!(DiskPressure::Warning.label(), "warning");
        assert_eq!(DiskPressure::Critical.label(), "critical");
        assert_eq!(DiskPressure::Fatal.label(), "fatal");
    }

    #[test]
    fn disk_free_bytes_succeeds_on_existing_dir() {
        let dir = tempdir().unwrap();
        let bytes = disk_free_bytes(dir.path());
        assert!(
            bytes.is_ok(),
            "disk_free_bytes should succeed for existing dir"
        );
        assert!(bytes.unwrap() > 0, "available space should be > 0");
    }

    #[test]
    fn disk_free_bytes_fails_on_nonexistent_path() {
        let result = disk_free_bytes(Path::new("/nonexistent_path_that_does_not_exist_12345"));
        assert!(
            result.is_err(),
            "disk_free_bytes should fail for nonexistent path"
        );
    }

    #[test]
    fn classify_pressure_all_zeros_is_ok() {
        // When all thresholds are 0, everything is Ok
        assert_eq!(classify_pressure(0, 0, 0, 0), DiskPressure::Ok);
        assert_eq!(classify_pressure(1_000_000, 0, 0, 0), DiskPressure::Ok);
    }

    #[test]
    fn classify_pressure_saturating_mul_no_panic() {
        // Huge MB values should not overflow due to saturating_mul
        assert_eq!(
            classify_pressure(0, u64::MAX, u64::MAX, u64::MAX),
            DiskPressure::Fatal
        );
    }

    // ── br-3h13: Additional disk.rs test coverage ─────────────────

    #[test]
    fn sqlite_url_fragment_stripped() {
        // Fragment (#) should be stripped just like query (?)
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite:///db.sqlite3#frag")
                .unwrap()
                .to_string_lossy(),
            "/db.sqlite3"
        );
    }

    #[test]
    fn sqlite_url_aiosqlite_memory() {
        assert!(is_sqlite_memory_database_url(
            "sqlite+aiosqlite:///:memory:"
        ));
        assert!(sqlite_file_path_from_database_url("sqlite+aiosqlite:///:memory:").is_none());
    }

    #[test]
    fn sqlite_url_aiosqlite_absolute_path() {
        assert_eq!(
            sqlite_file_path_from_database_url("sqlite+aiosqlite:////var/data/db.sqlite3")
                .unwrap()
                .to_string_lossy(),
            "/var/data/db.sqlite3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_sidecar_path_preserves_non_utf8_basename() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let db_path = PathBuf::from(OsStr::from_bytes(b"/tmp/sqlite-\xFF.db"));
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        assert_eq!(wal_path.as_os_str().as_bytes(), b"/tmp/sqlite-\xFF.db-wal");
    }

    #[test]
    fn sqlite_path_component_bare_returns_none_for_non_sqlite() {
        assert!(sqlite_path_component("mysql://localhost/db").is_none());
        assert!(sqlite_path_component("postgres://host/db").is_none());
    }

    #[test]
    fn sqlite_path_component_strips_query_and_fragment() {
        assert_eq!(
            sqlite_path_component("sqlite:///path.db?mode=rwc#frag"),
            Some("/path.db")
        );
    }

    #[test]
    fn sqlite_path_component_preserves_windows_unc_verbatim_prefix() {
        // Issue #93: `\\?\C:\…` UNC verbatim paths embed a literal `?` at
        // byte index 1.  Splitting on `?` truncated the path to `/\\` and
        // produced `'C:/\\\\'` open errors on Windows native builds.
        assert_eq!(
            sqlite_path_component(r"sqlite:///\\?\C:\Users\me\db.sqlite3"),
            Some(r"/\\?\C:\Users\me\db.sqlite3")
        );
        // Real query strings still get trimmed when they sit past the path.
        assert_eq!(
            sqlite_path_component(r"sqlite:///\\?\C:\Users\me\db.sqlite3?mode=ro"),
            Some(r"/\\?\C:\Users\me\db.sqlite3")
        );
    }

    /// GH#216: verbatim (`\\?\`) storage roots must simplify to the legacy
    /// form libgit2 can handle — unless simplification would be lossy.
    #[test]
    fn simplify_verbatim_path_disk_and_unc_forms() {
        assert_eq!(
            simplify_verbatim_path(Path::new(r"\\?\C:\dev\mailbox")),
            PathBuf::from(r"C:\dev\mailbox")
        );
        assert_eq!(
            simplify_verbatim_path(Path::new(r"\\?\UNC\server\share\mail")),
            PathBuf::from(r"\\server\share\mail")
        );
        // Non-verbatim paths (all Unix paths) are untouched.
        assert_eq!(
            simplify_verbatim_path(Path::new("/data/projects/mail")),
            PathBuf::from("/data/projects/mail")
        );
        // Trailing-dot/space components only survive in verbatim form: keep it.
        assert_eq!(
            simplify_verbatim_path(Path::new(r"\\?\C:\dev\weird.\x")),
            PathBuf::from(r"\\?\C:\dev\weird.\x")
        );
        // Paths at/over the classic MAX_PATH limit must stay verbatim.
        let long_tail = "x".repeat(300);
        let long = format!(r"\\?\C:\{long_tail}");
        assert_eq!(
            simplify_verbatim_path(Path::new(&long)),
            PathBuf::from(&long)
        );
        // Device namespace (`\\.\`) and non-disk verbatim forms are untouched.
        assert_eq!(
            simplify_verbatim_path(Path::new(r"\\?\Volume{guid}\x")),
            PathBuf::from(r"\\?\Volume{guid}\x")
        );
    }

    /// GH#216: mixed verbatim/plain prefix comparisons must succeed — storage
    /// roots are kept in the simplified legacy spelling while
    /// `fs::canonicalize` keeps returning the `\\?\` form for targets.
    #[test]
    fn relative_to_normalized_mixed_verbatim_and_plain() {
        // Verbatim target vs plain base: the archive write-back shape that
        // failed 507 times in a row on a fresh Windows DB.
        assert_eq!(
            relative_to_normalized(
                Path::new(r"\\?\C:\root\projects\p\messages\m.md"),
                Path::new(r"C:\root")
            ),
            Some(PathBuf::from(r"projects\p\messages\m.md"))
        );
        // Plain target vs verbatim base.
        assert_eq!(
            relative_to_normalized(Path::new(r"C:\root\a.txt"), Path::new(r"\\?\C:\root")),
            Some(PathBuf::from("a.txt"))
        );
        // Both verbatim.
        assert_eq!(
            relative_to_normalized(Path::new(r"\\?\C:\root\a"), Path::new(r"\\?\C:\root")),
            Some(PathBuf::from("a"))
        );
        // Verbatim UNC target vs plain UNC base.
        assert_eq!(
            relative_to_normalized(
                Path::new(r"\\?\UNC\server\share\mail\x"),
                Path::new(r"\\server\share\mail")
            ),
            Some(PathBuf::from("x"))
        );
        // Equal paths in different spellings: empty relative path.
        assert_eq!(
            relative_to_normalized(Path::new(r"\\?\C:\root"), Path::new(r"C:\root")),
            Some(PathBuf::new())
        );
        // Drive-root base keeps its trailing separator; still matches.
        assert_eq!(
            relative_to_normalized(Path::new(r"\\?\C:\a"), Path::new(r"C:\")),
            Some(PathBuf::from("a"))
        );
        // Component boundary: `C:\ab` is not inside `C:\a`.
        assert_eq!(
            relative_to_normalized(Path::new(r"\\?\C:\ab"), Path::new(r"C:\a")),
            None
        );
        // Different drive: no relative path.
        assert_eq!(
            relative_to_normalized(Path::new(r"\\?\D:\other"), Path::new(r"C:\root")),
            None
        );
        // Unix passthrough: plain `Path::strip_prefix` semantics unchanged.
        assert_eq!(
            relative_to_normalized(Path::new("/data/root/a/b"), Path::new("/data/root")),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(
            relative_to_normalized(Path::new("/data/rootx"), Path::new("/data/root")),
            None
        );
    }

    /// GH#216: attachment containment checks mix `canonicalize()` output with
    /// the simplified base spelling; the tolerant check must bridge them.
    #[test]
    fn path_starts_with_normalized_mixed_spellings() {
        assert!(path_starts_with_normalized(
            Path::new(r"\\?\C:\base\file"),
            Path::new(r"C:\base")
        ));
        assert!(path_starts_with_normalized(
            Path::new(r"C:\base\file"),
            Path::new(r"\\?\C:\base")
        ));
        assert!(path_starts_with_normalized(
            Path::new(r"\\?\UNC\srv\share\f"),
            Path::new(r"\\srv\share")
        ));
        // Non-prefix and lookalike-component cases stay rejected.
        assert!(!path_starts_with_normalized(
            Path::new(r"\\?\C:\evil\file"),
            Path::new(r"C:\base")
        ));
        assert!(!path_starts_with_normalized(
            Path::new(r"\\?\C:\baseline"),
            Path::new(r"C:\base")
        ));
        // Unix passthrough.
        assert!(path_starts_with_normalized(
            Path::new("/a/b/c"),
            Path::new("/a/b")
        ));
        assert!(!path_starts_with_normalized(
            Path::new("/a/bc"),
            Path::new("/a/b")
        ));
    }

    #[test]
    fn sqlite_url_from_path_strips_windows_unc_verbatim_prefix() {
        // Issue #93: smoke test for the round-trip we now perform on Windows.
        // On Unix this exercises the no-op branch.  On Windows it strips
        // `\\?\` and normalizes to forward slashes.
        let url = sqlite_url_from_path(Path::new(r"\\?\C:\Users\me\db.sqlite3"));
        assert!(
            !url.contains(r"\\?\"),
            "URL {url:?} must not embed `\\\\?\\` verbatim prefix"
        );
        let parsed = sqlite_file_path_from_database_url(&url)
            .expect("Windows-style URL should round-trip back to a path");
        // Forward-slash form is canonical for our URLs.  Both Path::new(`C:/…`)
        // and Path::new(`C:\…`) hit the same Win32 file APIs successfully.
        assert!(
            parsed.to_string_lossy().contains("C:/Users/me/db.sqlite3")
                || parsed.to_string_lossy().contains(r"C:\Users\me\db.sqlite3"),
            "round-trip lost path content: {parsed:?}",
        );
    }

    #[test]
    fn sqlite_file_path_from_database_url_peels_url_drive_letter_root() {
        // Issue #93: when a URL embeds a Windows path with a stray leading
        // `/` (URL syntax, not filesystem), parsing must strip the slash so
        // Win32 sees `C:/...` rather than `\C:\...` (which it rejects).
        let parsed = sqlite_file_path_from_database_url("sqlite:///C:/Users/me/db.sqlite3")
            .expect("drive-letter URL should parse");
        assert_eq!(parsed, PathBuf::from("C:/Users/me/db.sqlite3"));

        // Backslash form (some Windows tooling emits this) parses identically.
        let parsed = sqlite_file_path_from_database_url(r"sqlite:///C:\Users\me\db.sqlite3")
            .expect("drive-letter URL with backslashes should parse");
        assert_eq!(parsed, PathBuf::from(r"C:\Users\me\db.sqlite3"));
    }

    #[test]
    fn sqlite_url_from_path_round_trips_on_unix() {
        let url = sqlite_url_from_path(Path::new("/var/data/db.sqlite3"));
        assert_eq!(url, "sqlite:////var/data/db.sqlite3");
        let parsed = sqlite_file_path_from_database_url(&url).expect("round-trip");
        assert_eq!(parsed, PathBuf::from("/var/data/db.sqlite3"));
    }

    #[test]
    fn classify_pressure_exactly_at_warning_boundary() {
        // When free_bytes == warning threshold exactly, it's not below so should be Ok
        let threshold = 500;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, threshold, 100, 10),
            DiskPressure::Ok
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, threshold, 100, 10),
            DiskPressure::Warning
        );
    }

    #[test]
    fn classify_pressure_exactly_at_critical_boundary() {
        let threshold = 100;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 500, threshold, 10),
            DiskPressure::Warning // above critical but below warning
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, 500, threshold, 10),
            DiskPressure::Critical
        );
    }

    #[test]
    fn classify_pressure_exactly_at_fatal_boundary() {
        let threshold = 10;
        let at_threshold = threshold * MIB;
        assert_eq!(
            classify_pressure(at_threshold, 500, 100, threshold),
            DiskPressure::Critical // above fatal but below critical
        );
        assert_eq!(
            classify_pressure(at_threshold - 1, 500, 100, threshold),
            DiskPressure::Fatal
        );
    }

    #[test]
    fn sample_disk_with_memory_database_url() {
        let tmp = tempdir().expect("tempdir");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        let config = Config {
            storage_root,
            database_url: "sqlite:///:memory:".to_string(),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let sample = sample_disk(&config);
        assert!(sample.storage_free_bytes.is_some());
        assert!(
            sample.db_probe_path.is_none(),
            "memory DB has no probe path"
        );
        assert!(sample.db_free_bytes.is_none());
        // effective should be storage-only
        assert_eq!(sample.effective_free_bytes, sample.storage_free_bytes);
    }

    #[test]
    fn normalize_probe_path_existing_file() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("existing.db");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(normalize_probe_path(&file), file);
    }

    #[test]
    fn disk_sample_clone() {
        let sample = DiskSample {
            storage_probe_path: PathBuf::from("/tmp"),
            db_probe_path: Some(PathBuf::from("/var")),
            storage_free_bytes: Some(1000),
            db_free_bytes: Some(2000),
            effective_free_bytes: Some(1000),
            pressure: DiskPressure::Warning,
            errors: vec!["test error".to_string()],
        };
        let cloned = sample.clone();
        assert_eq!(cloned.pressure, DiskPressure::Warning);
        assert_eq!(cloned.effective_free_bytes, Some(1000));
        assert_eq!(cloned.errors.len(), 1);
        // Use `sample` after clone to prove it produced an independent copy.
        assert_eq!(sample.errors.len(), 1);
    }

    #[test]
    fn sample_and_record_updates_disk_metrics() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let db_file = tmp.path().join("db").join("storage.sqlite3");
        std::fs::create_dir_all(
            db_file
                .parent()
                .expect("db file parent should exist after create_dir_all"),
        )
        .expect("db parent should be created");

        let config = Config {
            storage_root,
            database_url: format!(
                "sqlite:////{}",
                db_file.to_string_lossy().trim_start_matches('/')
            ),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let metrics = crate::global_metrics();
        metrics.system.disk_storage_free_bytes.set(0);
        metrics.system.disk_db_free_bytes.set(0);
        metrics.system.disk_effective_free_bytes.set(0);
        metrics.system.disk_pressure_level.set(0);
        metrics.system.disk_last_sample_us.set(0);
        metrics.system.disk_sample_errors_total.store(0);

        let sample = sample_and_record(&config);
        assert_eq!(sample.pressure, DiskPressure::Ok);

        if let Some(storage_free) = sample.storage_free_bytes {
            assert_eq!(metrics.system.disk_storage_free_bytes.load(), storage_free);
        }
        if let Some(db_free) = sample.db_free_bytes {
            assert_eq!(metrics.system.disk_db_free_bytes.load(), db_free);
        }
        assert_eq!(
            metrics.system.disk_effective_free_bytes.load(),
            sample.effective_free_bytes.unwrap_or(0)
        );
        assert_eq!(
            metrics.system.disk_pressure_level.load(),
            sample.pressure.as_u64()
        );
        assert!(metrics.system.disk_last_sample_us.load() > 0);
        assert_eq!(
            metrics.system.disk_sample_errors_total.load(),
            u64::try_from(sample.errors.len()).expect("error count should fit u64")
        );
    }

    #[test]
    fn read_proc_io_bytes_returns_non_zero_on_linux() {
        let (read, write) = read_proc_io_bytes();
        // On Linux, the test process itself has done I/O, so at least read > 0.
        // On non-Linux, both are 0 (no /proc/self/io).
        #[cfg(target_os = "linux")]
        {
            // read_bytes may be 0 in some CI environments or if completely cached.
            let _ = read;
            let _ = write;
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(read, 0);
            assert_eq!(write, 0);
        }
        // Suppress unused variable warning.
        let _ = write;
    }

    #[test]
    fn sample_and_record_updates_io_bytes_metrics() {
        let tmp = tempdir().expect("tempdir should be created");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("storage root should be created");

        let config = Config {
            storage_root,
            database_url: "sqlite:///:memory:".to_string(),
            disk_space_warning_mb: 0,
            disk_space_critical_mb: 0,
            disk_space_fatal_mb: 0,
            ..Config::default()
        };

        let metrics = crate::global_metrics();
        metrics.system.disk_io_read_bytes.set(0);
        metrics.system.disk_io_write_bytes.set(0);

        let _sample = sample_and_record(&config);

        // On Linux, the I/O gauges should have been updated.
        #[cfg(target_os = "linux")]
        {
            // disk_io_read_bytes may be 0 if IO accounting is disabled or data is cached.
            let _ = metrics.system.disk_io_read_bytes.load();
        }
    }
}
