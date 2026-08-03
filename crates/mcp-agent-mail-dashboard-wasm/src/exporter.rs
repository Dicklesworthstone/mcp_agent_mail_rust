//! Read-only, snapshot-consistent aggregate reads for the demo-pack exporter.
//!
//! The exporter must never obtain write capability on the private source
//! mailbox, must not change source file bytes or the source-side namespace
//! while reading, and the aggregate counts it publishes must all describe one
//! database state even while a live mailbox is being written concurrently.
//! Ordinary raw reads may still update access-time metadata or trigger
//! filesystem audit side effects; metadata silence is outside this guarantee.
//! The three stated properties are enforced here rather than trusted:
//!
//! - SQLite never opens the source path. The database and any present WAL file
//!   are copied into a private temporary directory between two strong
//!   source fingerprints (file identity, metadata, and SHA-256). The copied
//!   bytes must exactly match the stable second fingerprint; orphaned SHM,
//!   rollback journals, path replacement, and concurrent source drift all fail
//!   closed before SQL runs;
//! - the verified private copy is opened with `SQLITE_OPEN_READONLY`, and that
//!   contract is proven at open time: a `PRAGMA user_version = <current>` header
//!   write — a semantic no-op that still travels the real write path — must be
//!   rejected with the exact `SQLITE_READONLY` code before any read runs;
//! - every aggregate query executes inside a single deferred read
//!   transaction on the verified copy, so all published counts describe one
//!   stable source snapshot without any source-side lock or shared-memory write.

#[cfg(not(unix))]
compile_error!(
    "the dashboard exporter currently requires Unix no-follow, link-count, and file-mode semantics"
);

use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use same_file::Handle;
use sha2::{Digest, Sha256};
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection, sqlite_error_code};

const MAX_SOURCE_DATABASE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SOURCE_WAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SOURCE_SHM_BYTES: u64 = 64 * 1024 * 1024;
const SOURCE_IO_ELAPSED_BUDGET: Duration = Duration::from_secs(30);
const SOURCE_IO_BUFFER_BYTES: usize = 64 * 1024;

const ACTIVE_RESERVATION_LEGACY_PREDICATE: &str = "fr.released_ts IS NULL \
    OR (typeof(fr.released_ts) IN ('integer', 'real') AND fr.released_ts <= 0) \
    OR (typeof(fr.released_ts) = 'text' AND lower(trim(fr.released_ts)) IN ('', '0', 'null', 'none')) \
    OR (typeof(fr.released_ts) = 'text' \
      AND length(trim(fr.released_ts)) > 0 \
      AND trim(fr.released_ts) GLOB '*[0-9]*' \
      AND REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(\
            trim(fr.released_ts),\
            '0',''),'1',''),'2',''),'3',''),'4',''),'5',''),'6',''),'7',''),'8',''),'9',''),'.',''),'+',''),'-','') = '' \
      AND CAST(trim(fr.released_ts) AS REAL) <= 0)";

/// The six public aggregate counts exported into the demo pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateCounts {
    pub projects: u64,
    pub agents: u64,
    pub messages: u64,
    pub file_reservations: u64,
    pub contact_links: u64,
    pub ack_pending: u64,
}

/// Filesystem identity for one validated source file.
#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    handle: Handle,
}

impl FileIdentity {
    fn from_file(file: &File) -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let metadata = file.metadata()?;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                handle: Handle::from_file(file.try_clone()?)?,
            })
        }
    }

    fn from_path_metadata(
        path: &Path,
        metadata: &Metadata,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let _ = path;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(Self {
                handle: Handle::from_path(path)?,
            })
        }
    }

    fn capture(
        path: &Path,
        label: &str,
        max_bytes: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (file, _, identity) = open_validated_source_file(path, label, max_bytes)?;
        drop(file);
        Ok(identity)
    }
}

/// Identity and strong content fingerprint for one source file.
#[derive(Debug, PartialEq, Eq)]
struct FileFingerprint {
    identity: FileIdentity,
    len: u64,
    modified: SystemTime,
    sha256: [u8; 32],
}

impl FileFingerprint {
    fn capture(
        path: &Path,
        label: &str,
        max_bytes: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let started_at = Instant::now();
        let (mut file, metadata, identity) = open_validated_source_file(path, label, max_bytes)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; SOURCE_IO_BUFFER_BYTES];
        let mut remaining = metadata.len();
        while remaining > 0 {
            ensure_source_io_budget(started_at, label)?;
            let requested = usize::try_from(remaining.min(buffer.len() as u64))?;
            let read = file.read(&mut buffer[..requested])?;
            if read == 0 {
                return Err(format!(
                    "{label} ended before its captured {}-byte length",
                    metadata.len()
                )
                .into());
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        ensure_source_io_budget(started_at, label)?;
        ensure_open_file_still_matches(&file, &identity, &metadata, label)?;
        Ok(Self {
            identity,
            len: metadata.len(),
            modified: metadata.modified()?,
            sha256: hasher.finalize().into(),
        })
    }

    fn content_matches(&self, other: &Self) -> bool {
        self.len == other.len && self.sha256 == other.sha256
    }
}

fn validate_source_metadata(
    metadata: &Metadata,
    label: &str,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if !metadata.file_type().is_file() {
        return Err(format!("{label} must be a regular file").into());
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{label} is {} bytes, exceeding the {max_bytes}-byte safety limit",
            metadata.len()
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(format!(
                "{label} has {} hard links; exactly one name is required so SQLite sidecars cannot be aliased",
                metadata.nlink()
            )
            .into());
        }
    }
    Ok(())
}

fn open_validated_source_file(
    path: &Path,
    label: &str,
    max_bytes: u64,
) -> Result<(File, Metadata, FileIdentity), Box<dyn std::error::Error>> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() {
        return Err(format!("{label} must not be a symbolic link").into());
    }
    validate_source_metadata(&path_metadata, label, max_bytes)?;
    let path_identity = FileIdentity::from_path_metadata(path, &path_metadata)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    validate_source_metadata(&metadata, label, max_bytes)?;
    let identity = FileIdentity::from_file(&file)?;
    if identity != path_identity {
        return Err(format!("{label} identity changed while it was being opened").into());
    }
    Ok((file, metadata, identity))
}

fn ensure_open_file_still_matches(
    file: &File,
    expected_identity: &FileIdentity,
    expected_metadata: &Metadata,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_metadata = file.metadata()?;
    let current_identity = FileIdentity::from_file(file)?;
    if current_identity != *expected_identity
        || current_metadata.len() != expected_metadata.len()
        || current_metadata.modified()? != expected_metadata.modified()?
    {
        return Err(format!("{label} changed while it was being read").into());
    }
    Ok(())
}

/// Check elapsed time between filesystem operations.
///
/// This is an I/O budget rather than a killable deadline: a single blocking
/// kernel/filesystem call can itself exceed the budget. Non-regular nodes are
/// rejected and Unix opens use `O_NONBLOCK`, but a stalled regular file on a
/// network or userspace filesystem still requires process-level supervision.
fn ensure_source_io_budget(
    started_at: Instant,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if started_at.elapsed() > SOURCE_IO_ELAPSED_BUDGET {
        return Err(format!(
            "{label} exceeded the {}-second elapsed I/O budget",
            SOURCE_IO_ELAPSED_BUDGET.as_secs()
        )
        .into());
    }
    Ok(())
}

fn copy_exact_fingerprint(
    source_path: &Path,
    destination_path: &Path,
    expected: &FileFingerprint,
    label: &str,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    let (mut source, metadata, identity) =
        open_validated_source_file(source_path, label, max_bytes)?;
    if identity != expected.identity
        || metadata.len() != expected.len
        || metadata.modified()? != expected.modified
    {
        return Err(format!(
            "{label} changed while creating the private snapshot; retry the export"
        )
        .into());
    }

    let mut destination_options = OpenOptions::new();
    destination_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        destination_options.mode(0o600);
    }
    let mut destination = destination_options.open(destination_path)?;
    validate_private_snapshot_file(&destination, destination_path, label)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; SOURCE_IO_BUFFER_BYTES];
    let mut remaining = expected.len;
    while remaining > 0 {
        ensure_source_io_budget(started_at, label)?;
        let requested = usize::try_from(remaining.min(buffer.len() as u64))?;
        let read = source.read(&mut buffer[..requested])?;
        if read == 0 {
            return Err(format!(
                "{label} ended before its captured {}-byte length",
                expected.len
            )
            .into());
        }
        destination.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    destination.sync_all()?;
    ensure_source_io_budget(started_at, label)?;
    ensure_open_file_still_matches(&source, &expected.identity, &metadata, label)?;
    let copied_sha256: [u8; 32] = hasher.finalize().into();
    if copied_sha256 != expected.sha256 || destination.metadata()?.len() != expected.len {
        return Err(
            format!("private {label} copy does not match the captured source bytes").into(),
        );
    }
    Ok(())
}

fn validate_private_snapshot_file(
    file: &File,
    path: &Path,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(format!("private {label} snapshot must be a regular file").into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(format!(
                "private {label} snapshot {} has mode {mode:o}, expected 600",
                path.display()
            )
            .into());
        }
        let parent = path
            .parent()
            .ok_or("private snapshot file has no parent directory")?;
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        if parent_metadata.uid() != metadata.uid() {
            return Err(format!(
                "private {label} snapshot owner does not match its staging directory"
            )
            .into());
        }
    }

    Ok(())
}

/// Stable source state around the private snapshot copy. SHM contents are not
/// logical database data and may change due to other readers; when present, its
/// file identity is still bound so a replaced SHM cannot cross the copy.
#[derive(Debug, PartialEq, Eq)]
struct SourceFingerprint {
    database: FileFingerprint,
    wal: Option<FileFingerprint>,
    shm: Option<FileIdentity>,
}

fn wal_sidecar_paths(path: &Path) -> (PathBuf, PathBuf) {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    (PathBuf::from(wal), PathBuf::from(shm))
}

fn rollback_journal_path(path: &Path) -> PathBuf {
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    PathBuf::from(journal)
}

fn absolute_path_without_following_leaf(
    path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let file_name = path
        .file_name()
        .ok_or("source database path has no file name")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Ok(parent.canonicalize()?.join(file_name))
}

impl SourceFingerprint {
    fn capture(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let (wal_path, shm_path) = wal_sidecar_paths(path);
        let journal_path = rollback_journal_path(path);
        if journal_path.try_exists()? {
            return Err(
                "source database has a rollback journal; retry after the writer transaction ends"
                    .into(),
            );
        }
        let wal_exists = wal_path.try_exists()?;
        let shm_exists = shm_path.try_exists()?;
        if shm_exists && !wal_exists {
            return Err(
                "source database has SHM without a WAL; retry after the writer stabilizes".into(),
            );
        }
        Ok(Self {
            database: FileFingerprint::capture(path, "source database", MAX_SOURCE_DATABASE_BYTES)?,
            wal: wal_exists
                .then(|| FileFingerprint::capture(&wal_path, "source WAL", MAX_SOURCE_WAL_BYTES))
                .transpose()?,
            shm: shm_exists
                .then(|| FileIdentity::capture(&shm_path, "source SHM", MAX_SOURCE_SHM_BYTES))
                .transpose()?,
        })
    }
}

struct VerifiedSourceSnapshot {
    directory: tempfile::TempDir,
    database_path: PathBuf,
}

fn canonical_temporary_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().canonicalize()?;
    if !root.is_absolute() {
        return Err("process temporary directory did not resolve to an absolute path".into());
    }
    let metadata = std::fs::symlink_metadata(&root)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("process temporary directory must resolve to a real directory".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let mode = metadata.mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(
                "process temporary directory is group/world writable without the sticky bit".into(),
            );
        }
    }

    Ok(root)
}

fn validate_private_snapshot_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("private snapshot staging path must be a real directory".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let mode = metadata.mode() & 0o777;
        if mode != 0o700 {
            return Err(format!(
                "private snapshot staging directory {} has mode {mode:o}, expected 700",
                path.display()
            )
            .into());
        }

        // A file created by this process carries its effective filesystem
        // owner. Retaining the empty probe until TempDir cleanup lets us verify
        // the newly created directory has the same owner without unsafe UID
        // syscalls or a platform-specific process API.
        let owner_probe_path = path.join(".agent-mail-snapshot-owner-check");
        let mut owner_probe_options = OpenOptions::new();
        owner_probe_options.write(true).create_new(true).mode(0o600);
        let owner_probe = owner_probe_options.open(&owner_probe_path)?;
        let owner_probe_metadata = owner_probe.metadata()?;
        if owner_probe_metadata.uid() != metadata.uid()
            || owner_probe_metadata.mode() & 0o777 != 0o600
        {
            return Err("private snapshot staging owner or owner-probe mode is unsafe".into());
        }
    }

    Ok(())
}

fn copy_verified_source(
    source_path: &Path,
) -> Result<VerifiedSourceSnapshot, Box<dyn std::error::Error>> {
    copy_verified_source_with_hook(source_path, || Ok(()))
}

fn copy_verified_source_with_hook<F>(
    source_path: &Path,
    after_initial_fingerprint: F,
) -> Result<VerifiedSourceSnapshot, Box<dyn std::error::Error>>
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
{
    let source_parent = source_path
        .parent()
        .ok_or("source database path has no parent directory")?;
    let temporary_root = canonical_temporary_root()?;
    if Handle::from_path(source_parent)? == Handle::from_path(&temporary_root)? {
        return Err(
            "source database is directly inside the process temporary directory; refusing to \
             create snapshot staging next to the source"
                .into(),
        );
    }
    let before = SourceFingerprint::capture(source_path)?;
    // Kept as an explicit seam so tests can force a source commit at the
    // security-critical point between the two fingerprints. Production uses
    // a no-op hook and pays no dynamic-dispatch cost.
    after_initial_fingerprint()?;
    let mut directory_builder = tempfile::Builder::new();
    directory_builder.prefix("agent-mail-dashboard-export-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        directory_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = directory_builder.tempdir_in(&temporary_root)?;
    if !directory.path().is_absolute() {
        return Err("private snapshot directory did not resolve to an absolute path".into());
    }
    validate_private_snapshot_directory(directory.path())?;
    let canonical_directory = directory.path().canonicalize()?;
    let database_path = canonical_directory.join("mailbox.sqlite3");
    copy_exact_fingerprint(
        source_path,
        &database_path,
        &before.database,
        "source database",
        MAX_SOURCE_DATABASE_BYTES,
    )?;

    let (source_wal, _) = wal_sidecar_paths(source_path);
    let (snapshot_wal, _) = wal_sidecar_paths(&database_path);
    if let Some(source_wal_fingerprint) = &before.wal {
        copy_exact_fingerprint(
            &source_wal,
            &snapshot_wal,
            source_wal_fingerprint,
            "source WAL",
            MAX_SOURCE_WAL_BYTES,
        )?;
    }

    let after = SourceFingerprint::capture(source_path)?;
    if after != before {
        return Err(
            "source database or WAL identity/content changed while creating the private snapshot; \
             retry the export"
                .into(),
        );
    }

    let copied_database = FileFingerprint::capture(
        &database_path,
        "private database snapshot",
        MAX_SOURCE_DATABASE_BYTES,
    )?;
    if !copied_database.content_matches(&after.database) {
        return Err("private database copy does not match the verified source bytes".into());
    }
    match &after.wal {
        Some(source_wal_fingerprint) => {
            let copied_wal = FileFingerprint::capture(
                &snapshot_wal,
                "private WAL snapshot",
                MAX_SOURCE_WAL_BYTES,
            )?;
            if !copied_wal.content_matches(source_wal_fingerprint) {
                return Err("private WAL copy does not match the verified source bytes".into());
            }
        }
        None if snapshot_wal.try_exists()? => {
            return Err("private snapshot unexpectedly contains a WAL file".into());
        }
        None => {}
    }

    Ok(VerifiedSourceSnapshot {
        directory,
        database_path,
    })
}

/// A strictly read-only handle on the source mailbox database.
pub struct SourceConnection {
    connection: SqliteConnection,
    _snapshot_directory: tempfile::TempDir,
    expected_user_version: i64,
    transaction_gate: Mutex<()>,
}

/// Open the source mailbox database strictly read-only, failing closed.
///
/// SQLite never opens `path`. A stable byte-for-byte snapshot is copied into a
/// private temporary directory first, and every SQLite operation targets only
/// that verified copy. This avoids source-side WAL shared-memory writes while
/// retaining committed WAL frames in the aggregate snapshot.
///
/// When the source has a live SQLite writer, run this exporter in a separate
/// process. POSIX advisory locks can be cancelled when an unrelated descriptor
/// for the same database is closed in the writer's process; this implementation
/// necessarily opens raw descriptors to hash and copy the source bytes.
///
/// In both modes the read-only contract is proven, not trusted: a
/// `PRAGMA user_version = <current>` header write — a semantic no-op that
/// still travels SQLite's real write path — must be rejected with
/// `SQLITE_READONLY`, otherwise the export aborts. (`BEGIN IMMEDIATE` is
/// deliberately not the probe — modern SQLite permits it on read-only
/// connections and only fails the first actual write.)
pub fn open_source_read_only(path: &Path) -> Result<SourceConnection, Box<dyn std::error::Error>> {
    // Canonicalize only the parent. Canonicalizing the complete caller path
    // would follow a leaf symlink before `symlink_metadata` and `O_NOFOLLOW`
    // can enforce the source-file identity contract.
    let source_path = absolute_path_without_following_leaf(path)?;
    let snapshot = copy_verified_source(&source_path)?;
    let canonical_snapshot_path = snapshot.database_path.canonicalize()?;
    if !canonical_snapshot_path.is_absolute() {
        return Err("private snapshot database path did not resolve to an absolute path".into());
    }
    let snapshot_path = canonical_snapshot_path
        .to_str()
        .ok_or("private snapshot path is not valid UTF-8")?
        .to_owned();
    let mut config = SqliteConfig::file(snapshot_path);
    config.flags = OpenFlags::read_only();
    let connection = SqliteConnection::open(&config)?;
    verify_opened_database_path(&connection, &canonical_snapshot_path)?;

    let user_version = read_user_version(&connection)?;
    match connection.execute_raw(&format!("PRAGMA user_version = {user_version}")) {
        Ok(()) => {
            return Err(
                "read-only contract not established: snapshot connection accepted a header write; \
                 refusing to export"
                    .into(),
            );
        }
        Err(error) => {
            let Some(code) = sqlite_error_code(&error) else {
                return Err(format!(
                    "read-only contract not established: header-write probe failed without an \
                     SQLite result code: {error}"
                )
                .into());
            };
            if code.extended() != sqlmodel_sqlite::ffi::SQLITE_READONLY {
                return Err(format!(
                    "read-only contract not established: header-write probe returned unexpected \
                     SQLite result code {} (extended {}): {error}",
                    code.primary(),
                    code.extended()
                )
                .into());
            }
        }
    }

    Ok(SourceConnection {
        connection,
        _snapshot_directory: snapshot.directory,
        expected_user_version: user_version,
        transaction_gate: Mutex::new(()),
    })
}

fn verify_opened_database_path(
    connection: &SqliteConnection,
    expected_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let rows = connection.query_sync("PRAGMA database_list", &[])?;
    let main_row = rows
        .iter()
        .find(|row| row.get_named::<String>("name").ok().as_deref() == Some("main"))
        .ok_or("SQLite did not report an opened main database")?;
    let opened_path = main_row.get_named::<String>("file")?;
    let canonical_opened_path = std::fs::canonicalize(Path::new(&opened_path))?;
    if Handle::from_path(&canonical_opened_path)? != Handle::from_path(expected_path)? {
        return Err(format!(
            "SQLite opened {}, not the verified private snapshot {}",
            canonical_opened_path.display(),
            expected_path.display()
        )
        .into());
    }
    Ok(())
}

fn read_user_version(connection: &SqliteConnection) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(connection
        .query_sync("PRAGMA user_version", &[])?
        .first()
        .ok_or("PRAGMA user_version returned no row")?
        .get_named::<i64>("user_version")?)
}

fn count(connection: &SqliteConnection, sql: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let rows = connection.query_sync(sql, &[])?;
    let value = rows
        .first()
        .ok_or("aggregate count query returned no row")?
        .get_named::<i64>("c")?;
    Ok(u64::try_from(value)?)
}

fn table_columns(
    connection: &SqliteConnection,
    table: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let pragma = match table {
        "file_reservations" => "PRAGMA table_info(file_reservations)",
        "file_reservation_releases" => "PRAGMA table_info(file_reservation_releases)",
        _ => return Err(format!("unsupported schema inspection table: {table}").into()),
    };
    let rows = connection.query_sync(pragma, &[])?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        columns.push(row.get_named::<String>("name")?);
    }
    Ok(columns)
}

fn count_active_reservations(
    connection: &SqliteConnection,
    now_micros: i64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let reservation_columns = table_columns(connection, "file_reservations")?;
    if !reservation_columns.iter().any(|column| column == "id")
        || !reservation_columns
            .iter()
            .any(|column| column == "expires_ts")
    {
        return Err("file_reservations schema is missing id or expires_ts".into());
    }
    let has_legacy_released_ts = reservation_columns
        .iter()
        .any(|column| column == "released_ts");

    let release_columns = table_columns(connection, "file_reservation_releases")?;
    let has_release_ledger = !release_columns.is_empty();
    if has_release_ledger
        && !release_columns
            .iter()
            .any(|column| column == "reservation_id")
    {
        return Err("file_reservation_releases schema is missing reservation_id".into());
    }

    let mut predicates = vec![format!("fr.expires_ts > {now_micros}")];
    if has_legacy_released_ts {
        predicates.push(format!("({ACTIVE_RESERVATION_LEGACY_PREDICATE})"));
    }
    if has_release_ledger {
        predicates.push(
            "NOT EXISTS (SELECT 1 FROM file_reservation_releases rr \
             WHERE rr.reservation_id = fr.id)"
                .to_string(),
        );
    }

    count(
        connection,
        &format!(
            "SELECT COUNT(*) AS c FROM file_reservations fr WHERE {}",
            predicates.join(" AND ")
        ),
    )
}

/// Read all six aggregate counts from one stable database snapshot.
///
/// Every query runs inside a single deferred read transaction: the snapshot
/// is established by the first read and held until the final one, so a
/// concurrent writer committing between individual queries can never produce
/// mutually inconsistent totals. The reservation-expiry cutoff is computed
/// once, so the whole read is a pure function of one database state.
///
/// Source bytes are no longer consulted here: [`open_source_read_only`] already
/// bound the connection to a verified private copy, so later mailbox writes
/// cannot tear or invalidate this snapshot.
pub fn read_aggregates_snapshot(
    source: &SourceConnection,
) -> Result<AggregateCounts, Box<dyn std::error::Error>> {
    let _transaction_guard = source
        .transaction_gate
        .lock()
        .map_err(|_| "aggregate transaction gate was poisoned")?;
    let connection = &source.connection;
    let now_micros = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;

    connection.execute_raw("BEGIN")?;
    let result =
        read_aggregates_in_transaction(connection, now_micros, source.expected_user_version);
    match result {
        Ok(counts) => match connection.execute_raw("COMMIT") {
            Ok(()) => Ok(counts),
            Err(error) => {
                // A failed COMMIT may leave the connection in a transaction.
                // Best-effort rollback restores it for a later caller; the
                // original COMMIT error remains the authoritative failure.
                let _ = connection.execute_raw("ROLLBACK");
                Err(error.into())
            }
        },
        Err(error) => {
            // The connection is read-only, so this only releases the snapshot.
            let _ = connection.execute_raw("ROLLBACK");
            Err(error)
        }
    }
}

fn read_aggregates_in_transaction(
    connection: &SqliteConnection,
    now_micros: i64,
    expected_user_version: i64,
) -> Result<AggregateCounts, Box<dyn std::error::Error>> {
    // This is deliberately the first read after BEGIN: it both establishes
    // the SQLite snapshot and binds all following counts to the schema version
    // whose read-only contract was probed at open time.
    let actual_user_version = read_user_version(connection)?;
    if actual_user_version != expected_user_version {
        return Err(format!(
            "private snapshot schema changed unexpectedly: expected user_version \
             {expected_user_version}, observed {actual_user_version}"
        )
        .into());
    }
    Ok(AggregateCounts {
        projects: count(connection, "SELECT COUNT(*) AS c FROM projects")?,
        agents: count(connection, "SELECT COUNT(*) AS c FROM agents")?,
        messages: count(connection, "SELECT COUNT(*) AS c FROM messages")?,
        file_reservations: count_active_reservations(connection, now_micros)?,
        contact_links: count(connection, "SELECT COUNT(*) AS c FROM agent_links")?,
        ack_pending: count(
            connection,
            "SELECT COUNT(*) AS c FROM message_recipients mr \
             JOIN messages m ON m.id = mr.message_id \
             WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    const FAR_FUTURE_MICROS: i64 = 4_102_444_800_000_000; // 2100-01-01

    fn create_schema(connection: &SqliteConnection) {
        for ddl in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, ack_required INTEGER NOT NULL)",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, \
             released_ts INTEGER, expires_ts INTEGER NOT NULL)",
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY)",
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, ack_ts INTEGER)",
        ] {
            connection.execute_raw(ddl).expect("create table");
        }
    }

    /// Insert one row into every aggregated table inside one transaction, so
    /// any single-snapshot read observes all six counts equal.
    fn insert_coherent_round(connection: &SqliteConnection) {
        connection
            .execute_raw("BEGIN IMMEDIATE")
            .expect("writer begin");
        for sql in [
            "INSERT INTO projects DEFAULT VALUES".to_string(),
            "INSERT INTO agents DEFAULT VALUES".to_string(),
            "INSERT INTO messages (ack_required) VALUES (1)".to_string(),
            format!(
                "INSERT INTO file_reservations (released_ts, expires_ts) \
                 VALUES (NULL, {FAR_FUTURE_MICROS})"
            ),
            "INSERT INTO agent_links DEFAULT VALUES".to_string(),
            "INSERT INTO message_recipients (message_id, ack_ts) \
             SELECT MAX(id), NULL FROM messages"
                .to_string(),
        ] {
            connection.execute_raw(&sql).expect("writer insert");
        }
        connection.execute_raw("COMMIT").expect("writer commit");
    }

    fn writer_db(path: &str) -> SqliteConnection {
        let connection = SqliteConnection::open_file(path.to_string()).expect("open writer db");
        connection
            .execute_raw("PRAGMA journal_mode=WAL")
            .expect("enable WAL");
        connection
    }

    fn snapshot_directory(path: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(path)
            .expect("read snapshot directory")
            .map(|entry| {
                let entry = entry.expect("read snapshot entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = std::fs::read(entry.path()).expect("read snapshot file");
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn read_only_open_rejects_missing_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.sqlite3");
        let result = open_source_read_only(&path);
        assert!(
            result.is_err(),
            "read-only open must not create a missing source database"
        );
        assert!(
            !path.exists(),
            "failed read-only open must not leave a file behind"
        );
    }

    #[test]
    fn read_only_open_rejects_shm_without_wal_without_changing_bytes_or_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();
        let writer = SqliteConnection::open_file(path_string.clone()).expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);

        let (_, shm_path) = wal_sidecar_paths(&path);
        std::fs::write(&shm_path, b"orphaned synthetic SHM").expect("create orphaned SHM fixture");
        let before = snapshot_directory(dir.path());

        let result = open_source_read_only(&path);
        let error = match result {
            Ok(_) => panic!("SHM-without-WAL source unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("SHM without a WAL"),
            "orphaned SHM failed for an unrelated reason: {error}"
        );
        assert_eq!(
            snapshot_directory(dir.path()),
            before,
            "rejected orphaned SHM must retain the same directory entries and bytes"
        );
    }

    #[test]
    fn read_only_open_accepts_recoverable_wal_without_shm() {
        let live_dir = tempfile::tempdir().expect("live tempdir");
        let live_path = live_dir.path().join("mailbox.sqlite3");
        let writer = writer_db(&live_path.to_string_lossy());
        create_schema(&writer);
        insert_coherent_round(&writer);
        let (live_wal, _) = wal_sidecar_paths(&live_path);
        assert!(
            live_wal.exists(),
            "fixture must retain committed WAL frames"
        );

        // Copy the stable database and WAL bytes into a separate source fixture
        // while deliberately omitting SHM. SQLite can reconstruct the WAL index
        // inside the exporter's writable private staging directory.
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source_path = source_dir.path().join("mailbox.sqlite3");
        let (source_wal, source_shm) = wal_sidecar_paths(&source_path);
        std::fs::copy(&live_path, &source_path).expect("copy database fixture");
        std::fs::copy(&live_wal, &source_wal).expect("copy WAL fixture");
        assert!(!source_shm.exists(), "source fixture must omit SHM");

        let reader = open_source_read_only(&source_path).expect("open WAL-only source");
        let counts = read_aggregates_snapshot(&reader).expect("read WAL-only aggregates");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let private_wal = reader
                ._snapshot_directory
                .path()
                .join("mailbox.sqlite3-wal");
            assert_eq!(
                private_wal.metadata().expect("private WAL metadata").mode() & 0o777,
                0o600
            );
        }
        drop(writer);
    }

    #[test]
    fn read_only_open_rejects_rollback_journals_without_changing_bytes_or_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();
        let writer = SqliteConnection::open_file(path_string.clone()).expect("open database");
        create_schema(&writer);
        drop(writer);

        let journal = rollback_journal_path(&path);
        std::fs::write(&journal, b"synthetic active rollback journal")
            .expect("create rollback-journal fixture");
        let before = snapshot_directory(dir.path());
        let error = match open_source_read_only(&path) {
            Ok(_) => panic!("rollback-journal source unexpectedly opened"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("source database has a rollback journal"),
            "rollback journal failed for an unrelated reason: {error}"
        );
        assert_eq!(snapshot_directory(dir.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_hard_linked_database_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let alias = dir.path().join("mailbox-alias.sqlite3");
        let writer = SqliteConnection::open_file(path.to_string_lossy().into_owned())
            .expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);
        std::fs::hard_link(&path, &alias).expect("create database hard link");

        let error = match open_source_read_only(&alias) {
            Ok(_) => panic!("hard-linked database alias unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("hard links"),
            "hard-linked database failed for an unrelated reason: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_database_leaf_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let alias = dir.path().join("mailbox-alias.sqlite3");
        let writer = SqliteConnection::open_file(path.to_string_lossy().into_owned())
            .expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);
        std::os::unix::fs::symlink(&path, &alias).expect("create database symlink");

        let error = match open_source_read_only(&alias) {
            Ok(_) => panic!("database leaf symlink unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("symbolic link"),
            "database symlink failed for an unrelated reason: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_special_and_oversized_source_nodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("mailbox-fifo.sqlite3");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo fixture setup failed: {status}");
        let fifo_error = match open_source_read_only(&fifo) {
            Ok(_) => panic!("FIFO source unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            fifo_error.to_string().contains("regular file"),
            "FIFO failed for an unrelated reason: {fifo_error}"
        );

        let device_error = match open_source_read_only(Path::new("/dev/zero")) {
            Ok(_) => panic!("unbounded device source unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            device_error.to_string().contains("regular file"),
            "device failed for an unrelated reason: {device_error}"
        );

        let oversized = dir.path().join("oversized.sqlite3");
        let oversized_file = File::create(&oversized).expect("create sparse oversized fixture");
        oversized_file
            .set_len(MAX_SOURCE_DATABASE_BYTES + 1)
            .expect("extend sparse oversized fixture");
        drop(oversized_file);
        let oversized_error = match open_source_read_only(&oversized) {
            Ok(_) => panic!("oversized source unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            oversized_error.to_string().contains("safety limit"),
            "oversized source failed for an unrelated reason: {oversized_error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_special_and_oversized_wal_sidecars() {
        let fifo_dir = tempfile::tempdir().expect("FIFO tempdir");
        let fifo_database = fifo_dir.path().join("mailbox.sqlite3");
        let fifo_writer = SqliteConnection::open_file(fifo_database.to_string_lossy().into_owned())
            .expect("open FIFO fixture database");
        create_schema(&fifo_writer);
        drop(fifo_writer);
        let (fifo_wal, fifo_shm) = wal_sidecar_paths(&fifo_database);
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_wal)
            .status()
            .expect("run mkfifo for WAL");
        assert!(
            status.success(),
            "WAL mkfifo fixture setup failed: {status}"
        );
        File::create(&fifo_shm).expect("create paired SHM fixture");
        let fifo_error = match open_source_read_only(&fifo_database) {
            Ok(_) => panic!("FIFO WAL unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            fifo_error.to_string().contains("regular file"),
            "FIFO WAL failed for an unrelated reason: {fifo_error}"
        );

        let symlink_dir = tempfile::tempdir().expect("symlink tempdir");
        let symlink_database = symlink_dir.path().join("mailbox.sqlite3");
        let symlink_writer =
            SqliteConnection::open_file(symlink_database.to_string_lossy().into_owned())
                .expect("open symlink fixture database");
        create_schema(&symlink_writer);
        drop(symlink_writer);
        let (symlink_wal, symlink_shm) = wal_sidecar_paths(&symlink_database);
        std::os::unix::fs::symlink("/dev/zero", &symlink_wal)
            .expect("create unbounded WAL symlink");
        File::create(&symlink_shm).expect("create paired SHM fixture");
        let symlink_error = match open_source_read_only(&symlink_database) {
            Ok(_) => panic!("symlink WAL unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            symlink_error.to_string().contains("symbolic link"),
            "symlink WAL failed for an unrelated reason: {symlink_error}"
        );

        let oversized_dir = tempfile::tempdir().expect("oversized tempdir");
        let oversized_database = oversized_dir.path().join("mailbox.sqlite3");
        let oversized_writer =
            SqliteConnection::open_file(oversized_database.to_string_lossy().into_owned())
                .expect("open oversized fixture database");
        create_schema(&oversized_writer);
        drop(oversized_writer);
        let (oversized_wal, oversized_shm) = wal_sidecar_paths(&oversized_database);
        let oversized_wal_file = File::create(&oversized_wal).expect("create oversized WAL");
        oversized_wal_file
            .set_len(MAX_SOURCE_WAL_BYTES + 1)
            .expect("extend sparse oversized WAL");
        drop(oversized_wal_file);
        File::create(&oversized_shm).expect("create paired SHM fixture");
        let oversized_error = match open_source_read_only(&oversized_database) {
            Ok(_) => panic!("oversized WAL unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            oversized_error.to_string().contains("safety limit"),
            "oversized WAL failed for an unrelated reason: {oversized_error}"
        );
    }

    // APFS rejects byte sequences that are not valid UTF-8 at rename time, so
    // exercise this Unix path contract on Linux (including the CI runner),
    // where arbitrary non-NUL filename bytes are supported.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_source_path_is_not_lossily_retargeted() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("mailbox.sqlite3");
        let writer = SqliteConnection::open_file(original.to_string_lossy().into_owned())
            .expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);

        let non_utf8 = dir
            .path()
            .join(OsString::from_vec(b"mailbox-\xff.sqlite3".to_vec()));
        std::fs::rename(&original, &non_utf8).expect("rename source to non-UTF-8 path");
        let reader = open_source_read_only(&non_utf8).expect("open non-UTF-8 source path");
        let counts = read_aggregates_snapshot(&reader).expect("read non-UTF-8 source aggregates");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
    }

    #[cfg(unix)]
    #[test]
    fn private_snapshot_and_cleanup_stay_absolute_under_uri_shaped_relative_tmpdir() {
        const CHILD_FLAG: &str = "AM_EXPORTER_URI_TMPDIR_CHILD";
        const SOURCE_PATH: &str = "AM_EXPORTER_URI_TMPDIR_SOURCE";

        if std::env::var_os(CHILD_FLAG).is_some() {
            let source = PathBuf::from(
                std::env::var_os(SOURCE_PATH).expect("child source path environment variable"),
            );
            let reader = open_source_read_only(&source).expect("open verified absolute snapshot");
            let snapshot_directory = reader._snapshot_directory.path().to_path_buf();
            assert!(
                snapshot_directory.is_absolute(),
                "private snapshot path must be absolute: {}",
                snapshot_directory.display()
            );
            let counts = read_aggregates_snapshot(&reader).expect("read intended source");
            assert_eq!(
                counts.projects, 1,
                "URI-shaped TMPDIR must not retarget SQLite to the decoy database"
            );

            // Recreate the path that a relative TempDir would target after a
            // CWD change. Dropping the reader must remove the real absolute
            // snapshot and leave this unrelated marker untouched.
            let alternate_cwd = source
                .parent()
                .expect("source parent")
                .join("alternate-cwd");
            let decoy_snapshot = alternate_cwd
                .join("file:decoy?mode=ro#")
                .join(snapshot_directory.file_name().expect("snapshot file name"));
            std::fs::create_dir_all(&decoy_snapshot).expect("create cleanup decoy");
            let decoy_marker = decoy_snapshot.join("unrelated-marker");
            std::fs::write(&decoy_marker, b"must survive").expect("write cleanup decoy marker");
            std::env::set_current_dir(&alternate_cwd).expect("change child working directory");
            drop(reader);
            assert!(
                !snapshot_directory.exists(),
                "absolute private snapshot must be cleaned after a CWD change"
            );
            assert!(
                decoy_marker.exists(),
                "TempDir cleanup must not retarget an unrelated relative path"
            );
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let source_writer = SqliteConnection::open_file(source.to_string_lossy().into_owned())
            .expect("open source database");
        create_schema(&source_writer);
        insert_coherent_round(&source_writer);
        drop(source_writer);

        let decoy = dir.path().join("decoy");
        let decoy_writer = SqliteConnection::open_file(decoy.to_string_lossy().into_owned())
            .expect("open decoy database");
        create_schema(&decoy_writer);
        insert_coherent_round(&decoy_writer);
        insert_coherent_round(&decoy_writer);
        drop(decoy_writer);

        let uri_shaped_tmpdir = dir.path().join("file:decoy?mode=ro#");
        std::fs::create_dir(&uri_shaped_tmpdir).expect("create URI-shaped temp root");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg(
                "exporter::tests::private_snapshot_and_cleanup_stay_absolute_under_uri_shaped_relative_tmpdir",
            )
            .arg("--nocapture")
            .current_dir(dir.path())
            .env("TMPDIR", "file:decoy?mode=ro#")
            .env(CHILD_FLAG, "1")
            .env(SOURCE_PATH, &source)
            .output()
            .expect("run isolated TMPDIR regression child");
        assert!(
            output.status.success(),
            "URI-shaped TMPDIR regression child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_snapshot_directory_and_database_are_owner_only() {
        use std::os::unix::fs::MetadataExt;

        let source_directory = tempfile::tempdir().expect("source tempdir");
        let source_path = source_directory.path().join("mailbox.sqlite3");
        let writer = SqliteConnection::open_file(source_path.to_string_lossy().into_owned())
            .expect("open source database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);

        let snapshot = copy_verified_source(&source_path).expect("copy verified source");
        let directory_metadata = snapshot
            .directory
            .path()
            .metadata()
            .expect("private directory metadata");
        let database_metadata = snapshot
            .database_path
            .metadata()
            .expect("private database metadata");
        assert_eq!(directory_metadata.mode() & 0o777, 0o700);
        assert_eq!(database_metadata.mode() & 0o777, 0o600);
        assert_eq!(directory_metadata.uid(), database_metadata.uid());
    }

    #[test]
    fn read_only_open_rejects_writes_and_write_locks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let writer = SqliteConnection::open_file(path.to_string_lossy().into_owned())
            .expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);

        let reader = open_source_read_only(&path).expect("read-only open");
        let write_attempt = reader
            .connection
            .execute_raw("INSERT INTO projects DEFAULT VALUES");
        let write_error = write_attempt.expect_err("read-only connection accepted a write");
        let write_code = sqlite_error_code(&write_error).expect("write rejection SQLite code");
        assert_eq!(
            write_code.extended(),
            sqlmodel_sqlite::ffi::SQLITE_READONLY,
            "the write probe must return exact base SQLITE_READONLY, not an extended READONLY condition"
        );
        let counts = read_aggregates_snapshot(&reader).expect("aggregates");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
    }

    #[test]
    fn active_reservation_count_matches_release_ledger_and_legacy_sentinels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let writer = SqliteConnection::open_file(path.to_string_lossy().into_owned())
            .expect("open database");
        create_schema(&writer);
        insert_coherent_round(&writer); // id=1: NULL, active
        writer
            .execute_raw(
                "CREATE TABLE file_reservation_releases (\
                    reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL\
                )",
            )
            .expect("create release ledger");
        for sql in [
            format!(
                "INSERT INTO file_reservations (released_ts, expires_ts) \
                 VALUES (0, {FAR_FUTURE_MICROS})"
            ),
            format!(
                "INSERT INTO file_reservations (released_ts, expires_ts) \
                 VALUES ('none', {FAR_FUTURE_MICROS})"
            ),
            format!(
                "INSERT INTO file_reservations (released_ts, expires_ts) \
                 VALUES (123, {FAR_FUTURE_MICROS})"
            ),
            format!(
                "INSERT INTO file_reservations (released_ts, expires_ts) \
                 VALUES (NULL, {FAR_FUTURE_MICROS})"
            ),
            "INSERT INTO file_reservations (released_ts, expires_ts) VALUES (NULL, 1)".to_string(),
            "INSERT INTO file_reservation_releases (reservation_id, released_ts) \
             VALUES (5, 999)"
                .to_string(),
        ] {
            writer
                .execute_raw(&sql)
                .expect("insert reservation fixture");
        }
        drop(writer);

        let reader = open_source_read_only(&path).expect("read-only open");
        let counts = read_aggregates_snapshot(&reader).expect("aggregates");
        assert_eq!(
            counts.file_reservations, 3,
            "NULL, numeric zero, and text 'none' are active; positive, expired, and ledger-released rows are not"
        );
    }

    #[test]
    fn active_reservation_count_supports_release_ledger_without_legacy_column() {
        let connection = SqliteConnection::open_memory().expect("in-memory database");
        connection
            .execute_raw(
                "CREATE TABLE file_reservations (\
                    id INTEGER PRIMARY KEY, expires_ts INTEGER NOT NULL\
                ); \
                 CREATE TABLE file_reservation_releases (\
                    reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL\
                 ); \
                 INSERT INTO file_reservations (id, expires_ts) VALUES (1, 1000), (2, 1000); \
                 INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (2, 50)",
            )
            .expect("create release-ledger-only schema");

        assert_eq!(
            count_active_reservations(&connection, 100).expect("count active reservations"),
            1
        );
    }

    #[test]
    fn aggregate_snapshot_is_immune_to_source_schema_change_after_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();
        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let reader = open_source_read_only(&path).expect("read-only open");
        writer
            .execute_raw("PRAGMA user_version = 42")
            .expect("simulate post-open schema migration");

        let counts = read_aggregates_snapshot(&reader)
            .expect("verified private snapshot should remain readable after source migration");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
    }

    #[test]
    fn private_snapshot_remains_stable_when_writer_appears_after_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);
        writer
            .execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint");
        drop(writer);

        // Capture one verified source state before a later writer appears.
        let reader = open_source_read_only(&path).expect("read-only open");

        // The writer changes only the source; the private snapshot remains the
        // exact state captured by open_source_read_only.
        let late_writer = writer_db(&path_string);
        insert_coherent_round(&late_writer);
        drop(late_writer);

        let counts = read_aggregates_snapshot(&reader).expect("stable private snapshot");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
    }

    #[test]
    fn source_commit_between_fingerprint_and_copy_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();
        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let error = match copy_verified_source_with_hook(&path, || {
            insert_coherent_round(&writer);
            Ok(())
        }) {
            Ok(_) => panic!("source drift during snapshot copy was accepted"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("changed while creating"),
            "source drift failed for an unrelated reason: {error}"
        );

        let source_rows = writer
            .query_sync("SELECT COUNT(*) AS c FROM projects", &[])
            .expect("source remains readable after rejected export");
        assert_eq!(source_rows[0].get_named::<i64>("c").expect("count"), 2);
    }

    #[test]
    fn private_snapshot_aggregates_remain_consistent_while_source_writes_continue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let reader = open_source_read_only(&path).expect("read-only open");
        let stop = Arc::new(AtomicBool::new(false));
        let first_commit = Arc::new(Barrier::new(2));
        let writer_stop = Arc::clone(&stop);
        let writer_first_commit = Arc::clone(&first_commit);
        let writer_path = path_string.clone();
        let writer_thread = std::thread::spawn(move || {
            let connection = writer_db(&writer_path);
            let mut rounds: u64 = 1; // schema setup already inserted round one
            insert_coherent_round(&connection);
            rounds += 1;
            writer_first_commit.wait();
            while !writer_stop.load(Ordering::Acquire) {
                insert_coherent_round(&connection);
                rounds += 1;
            }
            rounds
        });
        first_commit.wait();

        let mut observed = Vec::new();
        for _ in 0..200 {
            let counts = read_aggregates_snapshot(&reader).expect("aggregates");
            // The writer only ever commits one row to every table at once,
            // so any torn read across commits shows up as unequal counts.
            assert_eq!(
                [
                    counts.projects,
                    counts.agents,
                    counts.messages,
                    counts.file_reservations,
                    counts.contact_links,
                    counts.ack_pending,
                ],
                [counts.projects; 6],
                "aggregate counts were not read from a single database snapshot: {counts:?}"
            );
            observed.push(counts.projects);
        }
        stop.store(true, Ordering::Release);
        let rounds = writer_thread.join().expect("writer thread");

        assert!(
            observed.iter().all(|&count| count == 1),
            "the verified private snapshot must not drift with later source commits"
        );
        assert!(
            rounds > 1,
            "the source writer must commit after snapshot capture"
        );
    }

    #[test]
    fn live_wal_export_reads_committed_frames_without_changing_bytes_or_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);
        let (wal, shm) = wal_sidecar_paths(&path);
        assert!(
            wal.exists() && shm.exists(),
            "fixture must retain a live WAL pair"
        );

        let before = snapshot_directory(dir.path());
        let reader = open_source_read_only(&path).expect("read-only snapshot open");
        let counts = read_aggregates_snapshot(&reader).expect("aggregates from copied WAL");
        drop(reader);
        let after = snapshot_directory(dir.path());

        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "export-side reads must not create or remove live source sidecars"
        );
        assert_eq!(
            before, after,
            "export-side reads must not mutate live database, WAL, or SHM bytes"
        );

        drop(writer);
    }

    #[test]
    fn export_reads_do_not_change_source_bytes_or_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);
        // Fold the WAL back into the main file and drop the writer so the
        // directory is quiescent before the export-side read begins.
        writer
            .execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint");
        drop(writer);

        let before = snapshot_directory(dir.path());
        let reader = open_source_read_only(&path).expect("read-only open");
        let counts = read_aggregates_snapshot(&reader).expect("aggregates");
        drop(reader);
        let after = snapshot_directory(dir.path());

        assert_eq!(counts.projects, 1);
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>(),
            "export-side reads must not create or remove files next to the source"
        );
        assert_eq!(
            before, after,
            "export-side reads must not mutate any byte of the source files"
        );
    }
}
