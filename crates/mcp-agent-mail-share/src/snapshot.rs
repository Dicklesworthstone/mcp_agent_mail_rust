//! Step 1: SQLite snapshot creation for public exports and private archives.
//!
//! Creates an atomic, clean canonical SQLite copy of the source database suitable for
//! offline manipulation (scoping, scrubbing, finalization with FTS5/VACUUM).
//!
//! Public exports and scoped/scrubbed archives project the supported mailbox
//! tables through the runtime driver. Unscoped lossless archives use the source
//! engine's `VACUUM INTO` to preserve the complete database schema and payload.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
use sqlmodel_core::Value;

use crate::ShareError;

mod shared;

const SQLITE_SNAPSHOT_SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

/// Lifecycle of an opaque native archive phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullArchiveProgressState {
    Started,
    Progress,
    Finished,
    Failed,
}

/// Content-free phase timing for an unscoped, lossless database image.
///
/// Native exports emit phase boundaries only: `table` is always `None` and
/// `rows_copied` is zero. The source engine owns the copy statement, which cannot
/// be interrupted by this observer. Events contain no source paths or SQL.
#[derive(Debug, Clone, Copy)]
pub struct FullArchiveProgress {
    pub phase: &'static str,
    pub table: Option<&'static str>,
    pub rows_copied: u64,
    pub elapsed: Duration,
    pub state: FullArchiveProgressState,
}

/// Returning an error cancels at a phase boundary. A single database statement
/// is not preempted. The destination remains caller-owned staging until the
/// entire snapshot operation succeeds and must not be packaged after an error.
pub type FullArchiveObserver<'a> = dyn FnMut(FullArchiveProgress) -> Result<(), ShareError> + 'a;

fn full_archive_phase<T>(
    phase: &'static str,
    progress: &mut FullArchiveObserver<'_>,
    operation: impl FnOnce() -> Result<T, ShareError>,
) -> Result<T, ShareError> {
    let started = Instant::now();
    progress(FullArchiveProgress {
        phase,
        table: None,
        rows_copied: 0,
        elapsed: Duration::ZERO,
        state: FullArchiveProgressState::Started,
    })?;
    let result = operation();
    let event = FullArchiveProgress {
        phase,
        table: None,
        rows_copied: 0,
        elapsed: started.elapsed(),
        state: if result.is_ok() {
            FullArchiveProgressState::Finished
        } else {
            FullArchiveProgressState::Failed
        },
    };
    match result {
        Ok(value) => {
            progress(event)?;
            Ok(value)
        }
        Err(error) => {
            // Preserve the operation's original failure even if cancellation
            // is requested while reporting it.
            let _ = progress(event);
            Err(error)
        }
    }
}

/// A measured stage of snapshot preparation, independent of archive packaging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotProgressPhase {
    Copy,
    Scope,
    Scrub,
    Compact,
    Viewer,
}

/// Progress contains schema names and counts only, never mailbox contents.
///
/// `rows_copied` is cumulative within `table` for the current phase. A `None`
/// table marks a phase boundary; a table with zero rows marks its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotProgress {
    pub phase: SnapshotProgressPhase,
    pub table: Option<&'static str>,
    pub rows_copied: u64,
}

impl SnapshotProgress {
    pub(crate) const fn phase(phase: SnapshotProgressPhase) -> Self {
        Self {
            phase,
            table: None,
            rows_copied: 0,
        }
    }
}

/// Returning an error stops at the current checkpoint. Copy checkpoints occur
/// between bounded pages; a single database statement is not preempted.
pub type SnapshotObserver<'a> = dyn FnMut(SnapshotProgress) -> Result<(), ShareError> + 'a;

/// A bounded, content-free description of work in the snapshot pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPhase {
    /// Create the private database copy through the appropriate source engine.
    Snapshot,
    /// Resolve project selectors and remove out-of-scope rows from the copy.
    Filter,
    /// Apply the preset and publication-specific credential redaction.
    Scrub,
    /// Rebuild storage; published bundles also receive viewer search artifacts.
    Finalize,
}

/// Phase boundaries only: no database paths, selectors, SQL, or mailbox data.
///
/// A failed phase emits `Started` but not `Finished`; the returned error keeps
/// its existing type and is handled by the caller. Observers should perform
/// only bounded diagnostic work and must not mutate snapshot state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPhaseEvent {
    Started(SnapshotPhase),
    Finished(SnapshotPhase),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SnapshotDestinationProfile {
    #[default]
    Default,
    PortableExport,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SnapshotSourceProfile {
    #[default]
    Runtime,
    CanonicalExport,
}

#[cfg(test)]
// Export snapshots are canonical SQLite artifacts, so tests inspect them
// through the same engine external consumers use.
type SqliteConnection = CanonicalDbConn;

/// Known tables produced by the `mcp-agent-mail-db` schema.
///
/// Order matters: tables with foreign-key references must come after the
/// tables they reference so that data can be inserted without violating
/// constraints (when `PRAGMA foreign_keys = ON`).
#[derive(Clone, Copy)]
struct KnownTable {
    name: &'static str,
    page_by_column: Option<&'static str>,
    primary_key_columns: &'static [&'static str],
    columns: &'static [&'static str],
}

const KNOWN_TABLES: &[KnownTable] = &[
    KnownTable {
        name: "projects",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "slug", "human_key", "created_at"],
    },
    KnownTable {
        name: "products",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "product_uid", "name", "created_at"],
    },
    KnownTable {
        name: "product_project_links",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "product_id", "project_id", "created_at"],
    },
    KnownTable {
        name: "agents",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "project_id",
            "name",
            "program",
            "model",
            "task_description",
            "inception_ts",
            "last_active_ts",
            "attachments_policy",
            "contact_policy",
            "reaper_exempt",
            "registration_token",
            "retired_at",
        ],
    },
    KnownTable {
        name: "agent_deregistrations",
        page_by_column: Some("agent_id"),
        primary_key_columns: &["agent_id"],
        columns: &["agent_id", "deregistered_at"],
    },
    KnownTable {
        name: "messages",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "project_id",
            "sender_id",
            "thread_id",
            "topic",
            "subject",
            "body_md",
            "importance",
            "ack_required",
            "created_ts",
            "recipients_json",
            "attachments",
            // NULL is unknown legacy authority; {} proves a non-reply. Keep
            // the opaque value intact through private copies and finalization.
            "archive_metadata_json",
        ],
    },
    KnownTable {
        name: "message_recipients",
        page_by_column: None,
        primary_key_columns: &["message_id", "agent_id", "kind"],
        columns: &["message_id", "agent_id", "kind", "read_ts", "ack_ts"],
    },
    KnownTable {
        name: "file_reservations",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "project_id",
            "agent_id",
            "path_pattern",
            "exclusive",
            "reason",
            "created_ts",
            "expires_ts",
            "released_ts",
        ],
    },
    KnownTable {
        name: "file_reservation_releases",
        page_by_column: Some("reservation_id"),
        primary_key_columns: &["reservation_id"],
        columns: &["reservation_id", "released_ts"],
    },
    KnownTable {
        name: "agent_links",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "a_project_id",
            "a_agent_id",
            "b_project_id",
            "b_agent_id",
            "status",
            "reason",
            "created_ts",
            "updated_ts",
            "expires_ts",
        ],
    },
    KnownTable {
        name: "project_sibling_suggestions",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "project_a_id",
            "project_b_id",
            "score",
            "status",
            "rationale",
            "created_ts",
            "evaluated_ts",
            "confirmed_ts",
            "dismissed_ts",
        ],
    },
    KnownTable {
        name: "inbox_stats",
        page_by_column: Some("agent_id"),
        primary_key_columns: &["agent_id"],
        columns: &[
            "agent_id",
            "total_count",
            "unread_count",
            "ack_pending_count",
            "last_message_ts",
        ],
    },
    KnownTable {
        name: "tool_metrics_snapshots",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "collected_ts",
            "tool_name",
            "calls",
            "errors",
            "cluster",
            "capabilities_json",
            "complexity",
            "latency_avg_ms",
            "latency_min_ms",
            "latency_max_ms",
            "latency_p50_ms",
            "latency_p95_ms",
            "latency_p99_ms",
            "latency_is_slow",
        ],
    },
];

/// Create a snapshot of a live `FrankenSQLite` database at `destination`.
///
/// The source must already be admitted by `FrankenSQLite` and retain its complete
/// namespace sidecar pair. It is opened through the guarded, engine-enforced
/// read-only seam and copied inside one read transaction, so committed rows in
/// the primary file and WAL are observed as one consistent database snapshot.
/// The live source is never checkpointed or otherwise mutated. `checkpoint`
/// is retained for callers that share this signature with private canonical
/// snapshots, but has no effect for this live-source API.
///
/// Use [`create_private_canonical_sqlite_snapshot`] for a caller-owned,
/// engine-exclusive canonical SQLite artifact. There is intentionally no
/// namespace-absent fallback here: missing, incomplete, malformed, or stale
/// `FrankenSQLite` namespace authority fails closed.
///
/// Returns the destination path on success.
///
/// # Errors
///
/// - [`ShareError::SnapshotSourceNotFound`] if `source` does not exist.
/// - [`ShareError::SnapshotDestinationExists`] if `destination` or its SQLite
///   sidecar artifacts already exist.
/// - [`ShareError::Sqlite`] on any SQLite error.
/// - [`ShareError::Io`] on filesystem errors.
pub fn create_sqlite_snapshot(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
) -> Result<PathBuf, ShareError> {
    rebuild_sqlite_snapshot_with_profiles(
        source,
        destination,
        checkpoint,
        SnapshotSourceProfile::Runtime,
        SnapshotDestinationProfile::Default,
    )
}

/// Create a snapshot from a private, engine-exclusive canonical SQLite file.
///
/// This API may open the source with canonical SQLite and therefore must never
/// receive a live FrankenSQLite-managed mailbox path. It is intended for
/// caller-owned archive reconstructions and already-materialized snapshots.
/// When `checkpoint` is true, the private source is checkpointed through the
/// same canonical connection before its consistent read transaction begins.
///
/// # Errors
///
/// Returns [`ShareError`] when source validation, canonical SQLite access,
/// transfer, or publication fails.
pub fn create_private_canonical_sqlite_snapshot(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
) -> Result<PathBuf, ShareError> {
    rebuild_sqlite_snapshot_with_profiles(
        source,
        destination,
        checkpoint,
        SnapshotSourceProfile::CanonicalExport,
        SnapshotDestinationProfile::Default,
    )
}

pub(crate) fn rebuild_sqlite_snapshot_with_profiles(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
    source_profile: SnapshotSourceProfile,
    destination_profile: SnapshotDestinationProfile,
) -> Result<PathBuf, ShareError> {
    rebuild_sqlite_snapshot_with_progress(
        source,
        destination,
        checkpoint,
        source_profile,
        destination_profile,
        SnapshotProgressPhase::Copy,
        &mut |_| Ok(()),
    )
}

pub(crate) fn rebuild_sqlite_snapshot_with_progress(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
    source_profile: SnapshotSourceProfile,
    destination_profile: SnapshotDestinationProfile,
    phase: SnapshotProgressPhase,
    observer: &mut SnapshotObserver<'_>,
) -> Result<PathBuf, ShareError> {
    observer(SnapshotProgress::phase(phase))?;
    let source = crate::resolve_share_sqlite_path(source);

    match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(ShareError::Validation {
                message: format!("snapshot source is not a real file: {}", source.display()),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ShareError::SnapshotSourceNotFound {
                path: source.display().to_string(),
            });
        }
        Err(error) => return Err(ShareError::Io(error)),
    }
    if matches!(source_profile, SnapshotSourceProfile::CanonicalExport) {
        validate_private_canonical_snapshot_source(&source)?;
    }

    // Resolve destination to absolute path
    let dest = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };

    // Never overwrite
    match std::fs::symlink_metadata(&dest) {
        Ok(_) => {
            return Err(ShareError::SnapshotDestinationExists {
                path: dest.display().to_string(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ShareError::Io(error)),
    }
    if sqlite_sidecar_artifacts_exist(&dest)? {
        return Err(ShareError::SnapshotDestinationExists {
            path: dest.display().to_string(),
        });
    }

    // Create parent dirs
    if let Some(parent) = dest.parent() {
        ensure_real_directory(parent)?;
    }
    let dest_parent = dest.parent().ok_or_else(|| {
        ShareError::Io(std::io::Error::other(format!(
            "snapshot destination has no parent: {}",
            dest.display()
        )))
    })?;
    let stage_dir = tempfile::Builder::new()
        .prefix(".snapshot-stage.")
        .tempdir_in(dest_parent)
        .map_err(ShareError::Io)?;
    let staged_dest = stage_dir.path().join(
        dest.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("mailbox.sqlite3"),
    );

    let source_str = source.display().to_string();

    // Page size must be chosen before page 1 is initialized, so configure the
    // closed export profile before the first destination write.
    let dest_str = staged_dest.display().to_string();
    // The source is the live FrankenSQLite database, but the destination is a
    // disposable export image that will be published into place. Creating that
    // image through FrankenSQLite would bind persistent namespace records to
    // the staging pathname. Canonical SQLite has no pathname-bound namespace,
    // so it is the correct writer for portable export artifacts.
    let dst_conn = CanonicalDbConn::open_file(&dest_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot create destination DB {dest_str}: {e}"),
    })?;
    configure_destination(&dst_conn, &dest_str, destination_profile)?;
    configure_staging_durability(&dst_conn, &dest_str)?;

    match source_profile {
        SnapshotSourceProfile::Runtime => {
            let src = mcp_agent_mail_db::pool::open_guarded_read_only_franken_existing_file(
                &source,
                "share live SQLite snapshot",
            )
            .map_err(|e| ShareError::Sqlite {
                message: format!(
                    "cannot open live runtime source DB {} read-only: {e}",
                    source.display()
                ),
            })?;
            transfer_tables_with_progress(&src, &dst_conn, phase, observer)?;
            // A true read-only observer must use ordinary Drop. Checkpointing
            // close helpers are writer teardown and may mutate a live WAL.
            drop(src);
        }
        SnapshotSourceProfile::CanonicalExport => {
            let src = CanonicalDbConn::open_file(&source_str).map_err(|e| ShareError::Sqlite {
                message: format!("cannot open canonical export source DB {source_str}: {e}"),
            })?;
            if checkpoint {
                src.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
                    .map_err(|e| ShareError::Sqlite {
                        message: format!(
                            "cannot checkpoint private canonical source DB {source_str}: {e}"
                        ),
                    })?;
            }
            transfer_tables_with_progress(&src, &dst_conn, phase, observer)?;
        }
    }
    // Use the existing canonical staging connection for any destination
    // checkpoint. Opening the same inode with a second engine is unnecessary
    // and can invalidate process-wide classic fcntl locks on Unix.
    dst_conn
        .execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|e| ShareError::Sqlite {
            message: format!(
                "cannot checkpoint destination DB {} before publishing snapshot: {e}",
                staged_dest.display()
            ),
        })?;
    drop(dst_conn);
    observer(SnapshotProgress::phase(phase))?;
    publish_sqlite_snapshot(stage_dir, &staged_dest, &dest, sync_snapshot_parent)?;

    Ok(dest)
}

fn publish_sqlite_snapshot(
    stage_dir: tempfile::TempDir,
    staged: &Path,
    destination: &Path,
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ShareError> {
    let result = publish_sqlite_snapshot_image(staged, destination, sync_parent);
    if result.is_err() {
        // Keep the completed private image on collision or uncertain durability.
        // Never roll back a published name another process can now see.
        let _preserved_stage = stage_dir.keep();
    }
    result
}

fn publish_sqlite_snapshot_image(
    staged: &Path,
    destination: &Path,
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ShareError> {
    // Staging uses synchronous=OFF, so flush the completed image first. Windows
    // FlushFileBuffers requires write access; File::open is read-only there.
    let image = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(staged)?;
    image.sync_all()?;

    // Unlike rename, hard_link atomically refuses every occupied destination,
    // including dangling symlinks. Both names are on the same filesystem because
    // staging is created inside the destination parent. The private link remains
    // owned by the staging directory until the publication has been flushed.
    std::fs::hard_link(staged, destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            ShareError::SnapshotDestinationExists {
                path: destination.display().to_string(),
            }
        } else {
            ShareError::Io(error)
        }
    })?;
    // Flush file metadata after adding the link as well as its contents before.
    image.sync_all()?;
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("snapshot destination has no parent directory"))?;
    sync_parent(parent).map_err(|error| {
        ShareError::Io(std::io::Error::new(
            error.kind(),
            format!(
                "snapshot {} was published but parent durability could not be confirmed: {error}; \
                 private image retained at {}",
                destination.display(),
                staged.display()
            ),
        ))
    })
}

fn sync_snapshot_parent(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        // Portable std has no directory flush API on Windows. The image handle
        // is flushed before and after linking; do not claim a Unix directory
        // fsync guarantee on this platform.
        let _ = parent;
        Ok(())
    }
}

/// Relax durability on the staged destination while tables stream in.
///
/// br-gi4z3: the destination lives in a `.snapshot-stage.` tempdir that is
/// published only after checkpoint + `sync_all`, using atomic no-clobber linking.
/// Publication failures retain the private image. Per-statement durability
/// during staging buys nothing.
/// Without this, autocommit fsyncs made live-snapshot creation take minutes
/// (~2 fsyncs/row; 4,149 journal create/unlink events observed in a 2-minute
/// strace window on a 1,690-message mailbox).
fn configure_staging_durability(
    conn: &CanonicalDbConn,
    destination: &str,
) -> Result<(), ShareError> {
    conn.execute_raw("PRAGMA synchronous = OFF")
        .map_err(|error| ShareError::Sqlite {
            message: format!(
                "cannot configure staging destination DB {destination} with synchronous=OFF: \
                 {error}"
            ),
        })?;
    conn.execute_raw("PRAGMA journal_mode = MEMORY")
        .map_err(|error| ShareError::Sqlite {
            message: format!(
                "cannot configure staging destination DB {destination} with MEMORY journal \
                 mode: {error}"
            ),
        })?;
    Ok(())
}

fn configure_destination(
    conn: &CanonicalDbConn,
    destination: &str,
    profile: SnapshotDestinationProfile,
) -> Result<(), ShareError> {
    if !matches!(profile, SnapshotDestinationProfile::PortableExport) {
        return Ok(());
    }

    conn.execute_raw("PRAGMA page_size = 1024")
        .map_err(|error| ShareError::Sqlite {
            message: format!(
                "cannot configure destination DB {destination} with page size 1024: {error}"
            ),
        })?;
    conn.execute_raw("PRAGMA journal_mode='DELETE'")
        .map_err(|error| ShareError::Sqlite {
            message: format!(
                "cannot configure destination DB {destination} with DELETE journal mode: {error}"
            ),
        })?;
    Ok(())
}

fn sqlite_sidecar_artifacts_exist(path: &Path) -> Result<bool, ShareError> {
    for suffix in SQLITE_SNAPSHOT_SIDECAR_SUFFIXES {
        let mut sidecar_os = path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = PathBuf::from(sidecar_os);
        match std::fs::symlink_metadata(&sidecar_path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ShareError::Io(error)),
        }
    }
    Ok(false)
}

/// Enforce the engine-exclusive source contract before canonical SQLite can
/// open the main inode. Franken namespace authority means the path may be live
/// and must use the guarded same-engine API instead. A multiply-linked inode
/// likewise defeats pathname-based ownership checks because another alias can
/// still be live or replaced independently.
fn validate_private_canonical_snapshot_source(path: &Path) -> Result<(), ShareError> {
    let metadata = std::fs::symlink_metadata(path).map_err(ShareError::Io)?;
    if !metadata.file_type().is_file() {
        return Err(ShareError::Validation {
            message: format!(
                "private canonical snapshot source is not a regular file: {}",
                path.display()
            ),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(ShareError::Validation {
                message: format!(
                    "private canonical snapshot source must have exactly one hard link: {} has {}",
                    path.display(),
                    metadata.nlink()
                ),
            });
        }
    }

    for suffix in ["-fsqlite-ns-gate", "-fsqlite-ns-use"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        match std::fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                return Err(ShareError::Validation {
                    message: format!(
                        "private canonical snapshot source refuses FrankenSQLite namespace artifact {}",
                        sidecar.display()
                    ),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ShareError::Io(error)),
        }
    }

    Ok(())
}

/// How the guarded directory traversal in [`create_real_directory_all`]
/// failed. Callers map these onto their own error type/wording so the
/// share-crate guards can share one traversal (GH#230).
#[derive(Debug)]
pub(crate) enum RealDirError {
    /// The requested path contains a `..` component.
    ParentTraversal,
    /// An existing component is a symlink (and not a recognized macOS
    /// firmlink); the offending component path is attached.
    Symlink(PathBuf),
    /// An existing component is neither a directory nor a symlink; the
    /// offending component path is attached.
    NotDirectory(PathBuf),
    /// Filesystem inspection/creation failed.
    Io(std::io::Error),
}

/// Shared symlink-refusing `create_dir_all` used by every share-crate output
/// path guard (snapshot, bundle, static render, deploy, executor).
///
/// Walks `path` component by component, creating missing directories and
/// refusing `..` components, symlinks, and non-directory components.
///
/// GH#230: macOS firmlinks (`/var` -> `/private/var`, `/tmp` ->
/// `/private/tmp`, `/etc` -> `/private/etc`) are platform-canonical, not a
/// symlink-escape — rejecting them broke every operator-supplied output path
/// on macOS (TMPDIRs live under `/var/folders/...`). A component recognized
/// by [`mcp_agent_mail_core::disk::is_platform_temp_firmlink`] is resolved
/// and traversal continues against its canonical location; every OTHER
/// symlink stays a hard refusal.
pub(crate) fn create_real_directory_all(path: &Path) -> Result<(), RealDirError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        use std::path::Component;

        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(RealDirError::ParentTraversal),
            Component::Normal(segment) => {
                current.push(segment);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() {
                            if let Ok(resolved) = std::fs::canonicalize(&current)
                                && mcp_agent_mail_core::disk::is_platform_temp_firmlink(
                                    &current, &resolved,
                                )
                            {
                                current = resolved;
                                continue;
                            }
                            return Err(RealDirError::Symlink(current));
                        }
                        if !metadata.file_type().is_dir() {
                            return Err(RealDirError::NotDirectory(current));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&current).map_err(RealDirError::Io)?;
                    }
                    Err(error) => return Err(RealDirError::Io(error)),
                }
            }
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> std::io::Result<()> {
    match create_real_directory_all(path) {
        Ok(()) => Ok(()),
        Err(RealDirError::ParentTraversal) => Err(std::io::Error::other(format!(
            "refusing to create snapshot directory with parent traversal: {}",
            path.display()
        ))),
        Err(RealDirError::Symlink(current)) => Err(std::io::Error::other(format!(
            "refusing to traverse symlinked snapshot directory {}",
            current.display()
        ))),
        Err(RealDirError::NotDirectory(current)) => Err(std::io::Error::other(format!(
            "snapshot parent component is not a directory: {}",
            current.display()
        ))),
        Err(RealDirError::Io(error)) => Err(error),
    }
}

trait SnapshotSource {
    // Mirrors `DbConn::execute_raw`'s signature. Snapshot reads use an
    // explicit transaction so every table and pagination page observes one
    // primary-plus-WAL state.
    #[allow(clippy::result_large_err)]
    fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error>;

    // Mirrors `DbConn::query_sync`'s signature; `sqlmodel_core::Error`'s size
    // is upstream's choice and boxing here would diverge from that API.
    #[allow(clippy::result_large_err)]
    fn query_snapshot(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error>;
}

impl SnapshotSource for DbConn {
    fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
        self.execute_raw(sql)
    }

    fn query_snapshot(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
        self.query_sync(sql, params)
    }
}

impl SnapshotSource for CanonicalDbConn {
    fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
        self.execute_raw(sql)
    }

    fn query_snapshot(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
        self.query_sync(sql, params)
    }
}

/// Copy every known table while one source read transaction pins the database
/// snapshot. In WAL mode, separate queries without this transaction can each
/// observe a different commit and produce a cross-table or cross-page state
/// that never existed in the source.
#[cfg(test)]
fn transfer_tables_in_consistent_read<S: SnapshotSource>(
    src: &S,
    dst: &CanonicalDbConn,
) -> Result<(), ShareError> {
    transfer_tables_with_progress(src, dst, SnapshotProgressPhase::Copy, &mut |_| Ok(()))
}

fn transfer_tables_with_progress<S: SnapshotSource>(
    src: &S,
    dst: &CanonicalDbConn,
    phase: SnapshotProgressPhase,
    observer: &mut SnapshotObserver<'_>,
) -> Result<(), ShareError> {
    src.execute_snapshot("BEGIN")
        .map_err(|e| ShareError::Sqlite {
            message: format!("cannot begin consistent source snapshot read: {e}"),
        })?;

    match transfer_tables(src, dst, phase, observer) {
        Ok(()) => match src.execute_snapshot("COMMIT") {
            Ok(()) => Ok(()),
            Err(commit_error) => {
                let rollback_result = src.execute_snapshot("ROLLBACK");
                let rollback_suffix = rollback_result.err().map_or_else(String::new, |error| {
                    format!("; rollback after commit failure also failed: {error}")
                });
                Err(ShareError::Sqlite {
                    message: format!(
                        "cannot commit consistent source snapshot read: {commit_error}{rollback_suffix}"
                    ),
                })
            }
        },
        Err(transfer_error) => match src.execute_snapshot("ROLLBACK") {
            Ok(()) => Err(transfer_error),
            Err(rollback_error) => Err(ShareError::Sqlite {
                message: format!(
                    "snapshot transfer failed ({transfer_error}); source read transaction rollback also failed: {rollback_error}"
                ),
            }),
        },
    }
}

/// Transfer tables from a source snapshot to a fresh destination database.
fn transfer_tables<S: SnapshotSource>(
    src: &S,
    dst: &CanonicalDbConn,
    phase: SnapshotProgressPhase,
    observer: &mut SnapshotObserver<'_>,
) -> Result<(), ShareError> {
    for table in KNOWN_TABLES {
        let mut progress = SnapshotProgress {
            phase,
            table: Some(table.name),
            rows_copied: 0,
        };
        observer(progress)?;
        create_dst_table(dst, table)?;
        let source_columns = source_columns(src, table.name)?;
        if source_columns.is_empty() {
            continue;
        }
        let available_columns = available_columns(table, &source_columns);
        if available_columns.is_empty() {
            continue;
        }
        let insert_sql = build_insert(table.name, table.columns);
        let page_keys = snapshot_page_keys(src, table, &source_columns)?;
        // None is the first page, not an invented -1 identity. Imported
        // databases and previous snapshots can contain any signed SQLite ID.
        let mut after: Option<Vec<SnapshotKey>> = None;
        loop {
            let (select_sql, params) =
                snapshot_page_query(table, &available_columns, &page_keys, after.as_deref());

            let rows =
                src.query_snapshot(&select_sql, &params)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("SELECT from {} failed: {e}", table.name),
                    })?;

            if rows.len() > SNAPSHOT_PAGE_ROWS {
                return Err(ShareError::Validation {
                    message: format!("snapshot {} page exceeded its row bound", table.name),
                });
            }
            if rows.is_empty() {
                break;
            }

            // br-gi4z3: one transaction per page instead of one autocommit
            // (and, pre-staging-pragmas, ~2 fsyncs) per row. Error paths may
            // leave the transaction open: the staged destination is discarded
            // wholesale on failure, so no explicit ROLLBACK is needed.
            dst.execute_sync("BEGIN IMMEDIATE", &[])
                .map_err(|e| ShareError::Sqlite {
                    message: format!("BEGIN for {} page failed: {e}", table.name),
                })?;
            for row in &rows {
                let key = snapshot_row_key(row, table.name, &page_keys)?;
                if after.as_ref().is_some_and(|previous| key <= *previous) {
                    return Err(ShareError::Validation {
                        message: format!("snapshot {} cursor did not advance", table.name),
                    });
                }
                let values: Vec<Value> = table
                    .columns
                    .iter()
                    .map(|c| {
                        normalize_snapshot_value(
                            c,
                            row.get_by_name(c)
                                .cloned()
                                .or_else(|| snapshot_default_value(table.name, c))
                                .unwrap_or(Value::Null),
                        )
                    })
                    .collect();
                dst.execute_sync(&insert_sql, &values)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("INSERT into {} failed: {e}", table.name),
                    })?;
                after = Some(key);
            }
            dst.execute_sync("COMMIT", &[])
                .map_err(|e| ShareError::Sqlite {
                    message: format!("COMMIT for {} page failed: {e}", table.name),
                })?;
            progress.rows_copied = progress
                .rows_copied
                .saturating_add(u64::try_from(rows.len()).unwrap_or(u64::MAX));
            observer(progress)?;
        }
    }
    // Shared deliveries and lazy receipts are part of the same source read
    // transaction, including the finalization rebuild of a prepared snapshot.
    shared::transfer(src, dst, phase, observer)
}

/// Create a table in the destination database.
fn create_dst_table(dst: &CanonicalDbConn, table: &KnownTable) -> Result<(), ShareError> {
    let col_defs: Vec<String> = table.columns.iter().map(|c| format!("\"{c}\"")).collect();

    let pk_suffix = primary_key_suffix(table.primary_key_columns);

    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" ({}{pk_suffix})",
        table.name,
        col_defs.join(", ")
    );
    dst.execute_raw(&create_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("CREATE TABLE {} failed: {e}", table.name),
        })
}

/// Build an insert that cannot silently overwrite a previously copied row.
fn build_insert(table: &str, columns: &[&str]) -> String {
    let col_list = quoted_column_list(columns);
    let placeholders = (0..columns.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO \"{table}\" ({col_list}) VALUES ({placeholders})")
}

fn quoted_column_list(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn primary_key_suffix(primary_key_columns: &[&str]) -> String {
    if primary_key_columns.is_empty() {
        return String::new();
    }
    format!(", PRIMARY KEY({})", quoted_column_list(primary_key_columns))
}

fn source_columns<S: SnapshotSource>(src: &S, table: &str) -> Result<HashSet<String>, ShareError> {
    let rows = src
        .query_snapshot(&format!("PRAGMA table_info(\"{table}\")"), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA table_info({table}) failed: {e}"),
        })?;
    extract_column_names(&rows)
}

fn extract_column_names(rows: &[sqlmodel_core::Row]) -> Result<HashSet<String>, ShareError> {
    rows.iter()
        .map(|row| {
            row.get_named::<String>("name")
                .map(|name| name.to_ascii_lowercase())
                .map_err(|error| ShareError::Sqlite {
                    message: format!("cannot decode snapshot source column: {error}"),
                })
        })
        .collect()
}

const SNAPSHOT_PAGE_ROWS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SnapshotKey {
    Integer(i64),
    Text(String),
}

/// A keyset page needs an actual uniqueness constraint, not a promising column
/// name. Otherwise duplicate identities can straddle a page boundary and the
/// continuation would silently skip the second row. Runtime recipients use a
/// two-column PK; legacy recipients can add kind. Both fit the three-key walk.
fn snapshot_page_keys<'a>(
    src: &impl SnapshotSource,
    table: &'a KnownTable,
    columns: &HashSet<String>,
) -> Result<Vec<&'a str>, ShareError> {
    let keys = table
        .page_by_column
        .map_or_else(|| table.primary_key_columns.to_vec(), |column| vec![column]);
    if keys.is_empty() || keys.iter().any(|key| !columns.contains(*key)) {
        return Err(ShareError::Validation {
            message: format!("snapshot {} lacks its complete pagination key", table.name),
        });
    }
    let rows = src
        .query_snapshot(&format!("PRAGMA table_info(\"{}\")", table.name), &[])
        .map_err(|error| ShareError::Sqlite {
            message: format!("inspect snapshot {} key: {error}", table.name),
        })?;
    let mut primary = std::collections::BTreeMap::new();
    for row in rows {
        let name = row
            .get_named::<String>("name")
            .map_err(|error| ShareError::Sqlite {
                message: format!("decode snapshot {} key name: {error}", table.name),
            })?
            .to_ascii_lowercase();
        let position = row
            .get_named::<i64>("pk")
            .map_err(|error| ShareError::Sqlite {
                message: format!("decode snapshot {} key position: {error}", table.name),
            })?;
        if position < 0 || (position > 0 && primary.insert(position, name).is_some()) {
            return Err(ShareError::Validation {
                message: format!("snapshot {} has an ambiguous primary key", table.name),
            });
        }
    }
    if primary.is_empty()
        || primary.iter().enumerate().any(|(index, (position, name))| {
            *position != i64::try_from(index + 1).unwrap_or(i64::MAX)
                || !keys.contains(&name.as_str())
        })
    {
        return Err(ShareError::Validation {
            message: format!(
                "snapshot {} pagination key is not uniquely constrained",
                table.name
            ),
        });
    }
    Ok(keys)
}

fn snapshot_page_query(
    table: &KnownTable,
    columns: &[&str],
    keys: &[&str],
    after: Option<&[SnapshotKey]>,
) -> (String, Vec<Value>) {
    // Alias the projection so upper-case imported column declarations do not
    // change the names returned by the driver. Identifiers are known constants.
    let projection = columns
        .iter()
        .map(|column| format!("\"{column}\" AS \"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let order_keys = keys
        .iter()
        .map(|key| {
            if *key == "kind" {
                format!("\"{key}\" COLLATE BINARY")
            } else {
                format!("\"{key}\"")
            }
        })
        .collect::<Vec<_>>();
    let mut sql = format!("SELECT {projection} FROM \"{}\"", table.name);
    let mut params = Vec::new();
    if let Some(after) = after {
        debug_assert_eq!(after.len(), keys.len());
        let continuation = (0..keys.len())
            .map(|index| {
                let mut terms = (0..index)
                    .map(|equal| format!("{} = ?{}", order_keys[equal], equal + 1))
                    .collect::<Vec<_>>();
                terms.push(format!("{} > ?{}", order_keys[index], index + 1));
                format!("({})", terms.join(" AND "))
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        if keys.len() > 1 {
            // The disjunction alone can make SQLite walk the index from its
            // beginning on every page. Every matching tuple has its first
            // key >= the cursor's first key; expose that range explicitly so
            // the planner can seek past already-copied message populations.
            sql.push_str(&format!(
                " WHERE {} >= ?1 AND ({continuation})",
                order_keys[0]
            ));
        } else {
            sql.push_str(&format!(" WHERE {continuation}"));
        }
        params.extend(after.iter().map(|key| match key {
            SnapshotKey::Integer(value) => Value::BigInt(*value),
            SnapshotKey::Text(value) => Value::Text(value.clone()),
        }));
    }
    sql.push_str(&format!(
        " ORDER BY {} LIMIT {SNAPSHOT_PAGE_ROWS}",
        order_keys.join(", ")
    ));
    (sql, params)
}

fn snapshot_row_key(
    row: &sqlmodel_core::Row,
    table: &str,
    keys: &[&str],
) -> Result<Vec<SnapshotKey>, ShareError> {
    keys.iter()
        .map(|key| match (*key, row.get_by_name(key)) {
            ("kind", Some(Value::Text(value))) => Ok(SnapshotKey::Text(value.clone())),
            ("kind", _) => Err(ShareError::Validation {
                message: format!("unexpected non-text pagination column {table}.{key}"),
            }),
            (_, Some(Value::BigInt(value))) => Ok(SnapshotKey::Integer(*value)),
            (_, Some(Value::Int(value))) => Ok(SnapshotKey::Integer(i64::from(*value))),
            _ => Err(ShareError::Sqlite {
                message: format!("unexpected non-integer pagination column {table}.{key}"),
            }),
        })
        .collect()
}

fn available_columns<'a>(table: &'a KnownTable, source_columns: &HashSet<String>) -> Vec<&'a str> {
    table
        .columns
        .iter()
        .copied()
        .filter(|column| source_columns.contains(*column))
        .collect()
}

fn snapshot_default_value(table: &str, column: &str) -> Option<Value> {
    match (table, column) {
        ("agents", "reaper_exempt") => Some(Value::BigInt(0)),
        ("messages", "recipients_json") => Some(Value::Text("{}".to_string())),
        _ => None,
    }
}

fn normalize_snapshot_value(column: &str, value: Value) -> Value {
    if !snapshot_column_prefers_text(column) {
        return value;
    }
    match value {
        Value::Int(v) => Value::Text(v.to_string()),
        Value::BigInt(v) => Value::Text(v.to_string()),
        other => other,
    }
}

fn snapshot_column_prefers_text(column: &str) -> bool {
    column.ends_with("_ts") || column.ends_with("_at")
}

/// Full snapshot preparation pipeline for a live `FrankenSQLite` mailbox.
///
/// 1. Create snapshot
/// 2. Apply project scope
/// 3. Scrub data
/// 4. Compact storage; published shares also build viewer/search derivatives
///
/// The source assumptions are the same as [`create_sqlite_snapshot`]: a
/// complete, valid `FrankenSQLite` namespace pair is mandatory and the source
/// is opened only through the guarded read-only seam.
pub fn create_snapshot_context(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
) -> Result<SnapshotContext, ShareError> {
    create_snapshot_context_with_observer(
        source,
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        &mut |_| {},
    )
}

/// Prepare a live-mailbox snapshot while observing its phase boundaries.
///
/// Uses exactly the same guarded source and scrub policy as
/// [`create_snapshot_context`]. Observation does not enable any extra reads of
/// the source, release locks, or expose row contents to the observer.
///
/// # Errors
///
/// Returns the original [`ShareError`] if snapshot creation, project filtering,
/// scrubbing, or finalization fails. No later phase is started on failure.
pub fn create_snapshot_context_with_observer(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
    observer: &mut dyn FnMut(SnapshotPhaseEvent),
) -> Result<SnapshotContext, ShareError> {
    observer(SnapshotPhaseEvent::Started(SnapshotPhase::Snapshot));
    create_sqlite_snapshot(source, snapshot_path, true)?;
    observer(SnapshotPhaseEvent::Finished(SnapshotPhase::Snapshot));
    finish_snapshot_context(
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        observer,
        &mut |_| Ok(()),
    )
}

/// Prepare a live snapshot with phase and bounded-copy progress checkpoints.
///
/// The observer can cancel by returning an error. It runs synchronously, never
/// receives mailbox contents, and cannot interrupt an individual DB statement.
/// As with [`create_snapshot_context`], the destination is caller-owned staging
/// until the entire function succeeds and must not be published on an error.
///
/// # Errors
///
/// Returns snapshot preparation errors or the observer's cancellation error.
pub fn create_snapshot_context_with_progress(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
    observer: &mut SnapshotObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    rebuild_sqlite_snapshot_with_progress(
        source,
        snapshot_path,
        true,
        SnapshotSourceProfile::Runtime,
        SnapshotDestinationProfile::Default,
        SnapshotProgressPhase::Copy,
        observer,
    )?;
    finish_snapshot_context(
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        &mut |_| {},
        observer,
    )
}

/// What a snapshot is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPurpose {
    /// A share bundle that will be published: registration tokens and opaque
    /// message recovery metadata are removed whatever the scrub preset.
    Publish,
    /// A private archive (`am archive save`): the preset alone decides, so the
    /// lossless `archive` preset keeps tokens and a restore can still send.
    /// Viewer-only search indexes and materialized tables are not built.
    PrivateArchive,
}

/// Full snapshot preparation pipeline for a private canonical SQLite source.
///
/// The source must be a caller-owned, engine-exclusive archive reconstruction
/// or materialized snapshot. Never pass a live `FrankenSQLite` mailbox path;
/// use [`create_snapshot_context`] for that case.
///
/// # Errors
///
/// Returns [`ShareError`] when snapshot creation, project scoping, scrubbing,
/// or finalization fails.
pub fn create_private_canonical_snapshot_context(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
) -> Result<SnapshotContext, ShareError> {
    create_private_canonical_snapshot_context_with_observer(
        source,
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        &mut |_| {},
    )
}

/// Prepare a private canonical snapshot while observing its phase boundaries.
///
/// The source exclusivity requirement of
/// [`create_private_canonical_snapshot_context`] still applies. Events carry
/// only the fixed phase name and whether it started or finished.
///
/// # Errors
///
/// Returns the original [`ShareError`] if snapshot creation, project filtering,
/// scrubbing, or finalization fails. A failed phase never emits `Finished`.
pub fn create_private_canonical_snapshot_context_with_observer(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
    observer: &mut dyn FnMut(SnapshotPhaseEvent),
) -> Result<SnapshotContext, ShareError> {
    observer(SnapshotPhaseEvent::Started(SnapshotPhase::Snapshot));
    create_private_canonical_sqlite_snapshot(source, snapshot_path, true)?;
    observer(SnapshotPhaseEvent::Finished(SnapshotPhase::Snapshot));
    finish_snapshot_context(
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        observer,
        &mut |_| Ok(()),
    )
}

/// Prepare a private canonical snapshot with the same progress/cancellation
/// contract as [`create_snapshot_context_with_progress`]. Live mailbox paths
/// still require the guarded runtime-source API.
///
/// # Errors
///
/// Returns snapshot preparation errors or the observer's cancellation error.
pub fn create_private_canonical_snapshot_context_with_progress(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
    observer: &mut SnapshotObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    rebuild_sqlite_snapshot_with_progress(
        source,
        snapshot_path,
        true,
        SnapshotSourceProfile::CanonicalExport,
        SnapshotDestinationProfile::Default,
        SnapshotProgressPhase::Copy,
        observer,
    )?;
    finish_snapshot_context(
        snapshot_path,
        project_filters,
        scrub_preset,
        purpose,
        &mut |_| {},
        observer,
    )
}

/// Create a full, lossless private archive of a live FrankenSQLite database.
/// Unlike the share projection, this preserves every table, column, trigger,
/// index, sequence counter, and durable metadata such as `user_version` and
/// `application_id`. The private image is settled to DELETE journal mode for
/// portability. No scoping or scrub is applied.
///
/// The source requires valid existing FrankenSQLite namespace authority and is
/// opened through the engine-enforced read-only seam. Only the private image is
/// written. Use this API only for a private, unscoped archive.
///
/// # Errors
///
/// Returns an error on source admission, copy, validation, or publication
/// failure, including observer cancellation at phase boundaries. A database
/// statement cannot be preempted. The destination is caller-owned staging until
/// this function succeeds; a late cancellation may leave that image intact.
/// An existing destination is never replaced.
pub fn create_full_archive_snapshot_with_progress(
    source: &Path,
    destination: &Path,
    progress: &mut FullArchiveObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    create_full_archive_snapshot(
        source,
        destination,
        SnapshotSourceProfile::Runtime,
        progress,
    )
}

/// Create a full, lossless archive of a private canonical SQLite database.
///
/// The source must be caller-owned and engine-exclusive; live FrankenSQLite
/// mailboxes must use [`create_full_archive_snapshot_with_progress`].
/// No share-table allowlist, project scope, or scrubbing is applied.
///
/// # Errors
///
/// Returns an error on source admission, copy, validation, or publication
/// failure, including observer cancellation at phase boundaries. A database
/// statement cannot be preempted. The destination is caller-owned staging until
/// this function succeeds; a late cancellation may leave that image intact.
/// An existing destination is never replaced.
pub fn create_private_canonical_full_archive_snapshot_with_progress(
    source: &Path,
    destination: &Path,
    progress: &mut FullArchiveObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    create_full_archive_snapshot(
        source,
        destination,
        SnapshotSourceProfile::CanonicalExport,
        progress,
    )
}

/// An unscoped lossless backup is a database image, not a viewer projection.
/// Preserve schema, extra columns, triggers, indexes, sequence counters, and
/// database-only authority that the share allowlist cannot know about. The
/// source pager stays read-only; only the private destination is writable.
fn create_full_archive_snapshot(
    source: &Path,
    destination: &Path,
    source_profile: SnapshotSourceProfile,
    progress: &mut FullArchiveObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    let source = crate::resolve_share_sqlite_path(source);
    match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(ShareError::Validation {
                message: format!("snapshot source is not a real file: {}", source.display()),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ShareError::SnapshotSourceNotFound {
                path: source.display().to_string(),
            });
        }
        Err(error) => return Err(ShareError::Io(error)),
    }
    if matches!(source_profile, SnapshotSourceProfile::CanonicalExport) {
        validate_private_canonical_snapshot_source(&source)?;
    }
    let destination = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(ShareError::SnapshotDestinationExists {
                path: destination.display().to_string(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ShareError::Io(error)),
    }
    if sqlite_sidecar_artifacts_exist(&destination)? {
        return Err(ShareError::SnapshotDestinationExists {
            path: destination.display().to_string(),
        });
    }
    let parent = destination.parent().ok_or_else(|| {
        ShareError::Io(std::io::Error::other("snapshot destination has no parent"))
    })?;
    ensure_real_directory(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".archive-snapshot-stage.")
        .tempdir_in(parent)?;
    let staged = stage.path().join("mailbox.sqlite3");
    let staged_text = staged.to_str().ok_or_else(|| ShareError::Validation {
        message: "snapshot destination must be a UTF-8 path".to_string(),
    })?;

    let conn = full_archive_phase("snapshot_source_open", progress, || {
        use mcp_agent_mail_db::pool::{
            GuardedReadOnlyConn, open_guarded_read_only_canonical_sqlite_file,
            open_guarded_read_only_franken_existing_file,
        };
        match source_profile {
            SnapshotSourceProfile::Runtime => {
                open_guarded_read_only_franken_existing_file(&source, "full archive snapshot")
                    .map(GuardedReadOnlyConn::Franken)
            }
            SnapshotSourceProfile::CanonicalExport => {
                open_guarded_read_only_canonical_sqlite_file(&source, "full archive snapshot")
                    .map(GuardedReadOnlyConn::Canonical)
            }
        }
        .map_err(|error| ShareError::Sqlite {
            message: format!("cannot open full archive source read-only: {error}"),
        })
    })?;
    full_archive_phase("snapshot_full_copy", progress, || {
        // This changes the SQL policy only. Both guarded openers retain an
        // engine-enforced read-only pager. VACUUM INTO takes one consistent
        // read of primary + committed WAL, writes a fresh private inode, and
        // neither checkpoints nor rewrites the source family.
        conn.execute_raw("PRAGMA query_only = OFF")
            .and_then(|()| {
                conn.execute_raw(&format!(
                    "VACUUM INTO '{}'",
                    staged_text.replace('\'', "''")
                ))
            })
            .map_err(|error| ShareError::Sqlite {
                message: format!("full archive database copy failed: {error}"),
            })
    })?;
    drop(conn);
    full_archive_phase("snapshot_neutralize", progress, || {
        // Same private-image handoff used by doctor and recovery. Retire only
        // destination namespace witnesses and fold its WAL, then allow the
        // canonical validator to inspect that private inode.
        mcp_agent_mail_db::neutralize_private_salvage_artifact(&staged, staged_text).map_err(
            |error| ShareError::Sqlite {
                message: format!("cannot settle full archive snapshot: {error}"),
            },
        )
    })?;
    let mut context = full_archive_phase("snapshot_validate", progress, || {
        full_archive_context(&staged)
    })?;
    full_archive_phase("snapshot_publish", progress, || {
        publish_sqlite_snapshot(stage, &staged, &destination, sync_snapshot_parent)
    })?;
    context.snapshot_path = destination;
    Ok(context)
}

fn full_archive_context(path: &Path) -> Result<SnapshotContext, ShareError> {
    let conn = mcp_agent_mail_db::pool::open_guarded_read_only_canonical_sqlite_file(
        path,
        "full archive validation",
    )
    .map_err(|error| ShareError::Sqlite {
        message: format!("cannot read completed full archive snapshot: {error}"),
    })?;
    let sql_error = |error| ShareError::Sqlite {
        message: format!("cannot validate full archive snapshot: {error}"),
    };
    let integrity = conn
        .query_sync("PRAGMA quick_check", &[])
        .map_err(sql_error)?;
    if integrity.len() != 1
        || integrity[0].get_by_name("quick_check") != Some(&Value::Text("ok".to_string()))
    {
        return Err(ShareError::Validation {
            message: "full archive snapshot failed SQLite quick_check".to_string(),
        });
    }
    let tables: HashSet<String> = conn
        .query_sync("SELECT name FROM sqlite_master WHERE type = 'table'", &[])
        .map_err(sql_error)?
        .iter()
        .map(|row| {
            row.get_named::<String>("name")
                .map(|name| name.to_ascii_lowercase())
        })
        .collect::<Result<_, _>>()
        .map_err(sql_error)?;
    let count = |table: &str| -> Result<i64, ShareError> {
        if !tables.contains(table) {
            return Ok(0);
        }
        // Callers below pass only fixed mailbox table identifiers.
        let rows = conn
            .query_sync(&format!("SELECT COUNT(*) AS n FROM \"{table}\""), &[])
            .map_err(sql_error)?;
        rows.first()
            .ok_or_else(|| ShareError::Validation {
                message: format!("full archive snapshot count is missing for {table}"),
            })?
            .get_named("n")
            .map_err(sql_error)
    };
    let mut projects = Vec::new();
    if tables.contains("projects") {
        for row in conn
            .query_sync(
                "SELECT id AS id, slug AS slug, human_key AS human_key FROM projects ORDER BY id",
                &[],
            )
            .map_err(sql_error)?
        {
            projects.push(crate::scope::ProjectRecord {
                id: row.get_named("id").map_err(sql_error)?,
                slug: row.get_named("slug").map_err(sql_error)?,
                human_key: row.get_named("human_key").map_err(sql_error)?,
            });
        }
    }
    let remaining = crate::scope::RemainingCounts {
        projects: count("projects")?,
        agents: count("agents")?,
        messages: count("messages")?,
        recipients: count("message_recipients")?,
        file_reservations: count("file_reservations")?,
        inbox_stats: count("inbox_stats")?,
        agent_links: count("agent_links")?,
        project_sibling_suggestions: count("project_sibling_suggestions")?,
    };
    let scrub_summary = crate::scrub::ScrubSummary {
        preset: "archive".to_string(),
        pseudonym_salt: "archive".to_string(),
        agents_total: remaining.agents,
        agents_pseudonymized: 0,
        ack_flags_cleared: 0,
        recipients_cleared: 0,
        file_reservations_removed: 0,
        agent_links_removed: 0,
        secrets_replaced: 0,
        attachments_sanitized: 0,
        bodies_redacted: 0,
        attachments_cleared: 0,
    };
    Ok(SnapshotContext {
        snapshot_path: path.to_path_buf(),
        scope: crate::scope::ProjectScopeResult {
            identifiers: Vec::new(),
            projects,
            removed_count: 0,
            remaining,
        },
        scrub_summary,
        // No viewer index was built. Any existing runtime FTS schema is still
        // preserved verbatim as part of the full database image.
        fts_enabled: false,
    })
}

fn finish_snapshot_context(
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
    purpose: SnapshotPurpose,
    phase_observer: &mut dyn FnMut(SnapshotPhaseEvent),
    observer: &mut SnapshotObserver<'_>,
) -> Result<SnapshotContext, ShareError> {
    phase_observer(SnapshotPhaseEvent::Started(SnapshotPhase::Filter));
    observer(SnapshotProgress::phase(SnapshotProgressPhase::Scope))?;
    // Scoping deletes the other projects first; keep their paths so the scrub
    // still redacts them where kept rows mention them.
    let all_project_paths = if project_filters.is_empty() {
        Vec::new()
    } else {
        crate::scope::project_paths_before_scope(snapshot_path)?
    };
    let mut scope = crate::apply_project_scope(snapshot_path, project_filters)?;
    phase_observer(SnapshotPhaseEvent::Finished(SnapshotPhase::Filter));
    phase_observer(SnapshotPhaseEvent::Started(SnapshotPhase::Scrub));
    observer(SnapshotProgress::phase(SnapshotProgressPhase::Scrub))?;
    let mut scrub_summary =
        crate::scrub::scrub_snapshot_redacting(snapshot_path, scrub_preset, &all_project_paths)?;
    if purpose == SnapshotPurpose::Publish {
        // Before finalize rebuilds the file, so no page keeps the old values.
        let removed = crate::scrub::clear_registration_tokens(snapshot_path)?;
        scrub_summary.secrets_replaced += removed;
        clear_published_archive_metadata(snapshot_path)?;
    }
    if !matches!(scrub_preset, crate::ScrubPreset::Archive) {
        crate::scrub::redact_scope_project_human_keys(&mut scope);
    }
    phase_observer(SnapshotPhaseEvent::Finished(SnapshotPhase::Scrub));
    phase_observer(SnapshotPhaseEvent::Started(SnapshotPhase::Finalize));
    // A restorable private archive needs the authoritative tables, not the
    // static viewer's correlated recipient projections, attachment expansion,
    // FTS index or analysis pass. Still rebuild the scoped/scrubbed image so
    // deleted rows and redacted payload bytes cannot remain in free pages.
    let fts_enabled = match purpose {
        SnapshotPurpose::PrivateArchive => {
            crate::finalize::finalize_snapshot_with_progress(snapshot_path, observer)?;
            false
        }
        SnapshotPurpose::Publish => {
            crate::finalize::finalize_export_with_progress(snapshot_path, observer)?.fts_enabled
        }
    };
    phase_observer(SnapshotPhaseEvent::Finished(SnapshotPhase::Finalize));

    Ok(SnapshotContext {
        snapshot_path: snapshot_path.to_path_buf(),
        scope,
        scrub_summary,
        fts_enabled,
    })
}

/// Recovery metadata is not viewer data and newer writers may put credentials
/// or private routing in it. Preserve it in private archives, but never publish
/// it, including with the otherwise lossless archive scrub preset. This runs
/// before the final canonical rebuild so old payload pages are not shipped.
fn clear_published_archive_metadata(snapshot_path: &Path) -> Result<(), ShareError> {
    let path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let conn =
        DbConn::open_file(path.display().to_string()).map_err(|error| ShareError::Sqlite {
            message: format!("open snapshot for recovery metadata cleanup: {error}"),
        })?;
    let conn = mcp_agent_mail_db::guard_db_conn(conn, "published recovery metadata cleanup");
    // Every snapshot produced here has this column, even for legacy sources.
    // Its absence or an unreadable schema must stop publication, not skip it.
    conn.execute_raw(
        "UPDATE messages SET archive_metadata_json = NULL WHERE archive_metadata_json IS NOT NULL",
    )
    .map_err(|error| ShareError::Sqlite {
        message: format!("clear published recovery metadata: {error}"),
    })?;
    let remaining = conn
        .query_sync(
            "SELECT 1 FROM messages WHERE archive_metadata_json IS NOT NULL LIMIT 1",
            &[],
        )
        .map_err(|error| ShareError::Sqlite {
            message: format!("verify published recovery metadata cleanup: {error}"),
        })?;
    if !remaining.is_empty() {
        return Err(ShareError::Validation {
            message: "message recovery metadata remains in published snapshot".to_string(),
        });
    }
    Ok(())
}

/// Context returned by the snapshot preparation pipeline.
#[derive(Debug, Clone)]
pub struct SnapshotContext {
    pub snapshot_path: PathBuf,
    pub scope: crate::scope::ProjectScopeResult,
    pub scrub_summary: crate::scrub::ScrubSummary,
    pub fts_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_archive_preserves_schema_authority_and_unknown_payloads() {
        for live in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("full-source.sqlite3");
            let destination = directory.path().join("full-copy.sqlite3");
            let seed = |conn: &dyn SnapshotSource| {
                for sql in [
                    "PRAGMA user_version=231",
                    "PRAGMA application_id=4242",
                    "CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
                    "CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, registration_token TEXT, operator_note TEXT)",
                    "CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER, body_md TEXT, archive_metadata_json TEXT)",
                    "CREATE TABLE db_identity(generation TEXT NOT NULL)",
                    "CREATE TABLE future_state(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, payload BLOB NOT NULL, extra TEXT DEFAULT 'kept')",
                    "CREATE INDEX future_payload ON future_state(payload)",
                    "CREATE TABLE backup_audit(value INTEGER)",
                    "CREATE TRIGGER future_after_insert AFTER INSERT ON future_state BEGIN INSERT INTO backup_audit VALUES(NEW.id); END",
                    "INSERT INTO projects VALUES(1,'backup','/backup',17)",
                    "INSERT INTO agents VALUES(2,1,'BlueLake','registration-secret','extra authoritative column')",
                    "INSERT INTO messages VALUES(3,1,'keep exact body','{\"reply_to\":1}')",
                    "INSERT INTO db_identity VALUES('durable-generation')",
                    "INSERT INTO future_state(name,payload) VALUES('one',X'00FF')",
                    "UPDATE sqlite_sequence SET seq=500 WHERE name='future_state'",
                ] {
                    conn.execute_snapshot(sql).unwrap();
                }
            };
            if live {
                let conn = DbConn::open_file(source.display().to_string()).unwrap();
                seed(&conn);
            } else {
                seed(&private_source(&source));
            }
            let source_before = std::fs::read(&source).unwrap();
            let mut events = Vec::new();
            let context = if live {
                create_full_archive_snapshot_with_progress(&source, &destination, &mut |event| {
                    events.push(event);
                    Ok(())
                })
            } else {
                create_private_canonical_full_archive_snapshot_with_progress(
                    &source,
                    &destination,
                    &mut |event| {
                        events.push(event);
                        Ok(())
                    },
                )
            }
            .unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), source_before);
            assert!(!context.fts_enabled);
            assert_eq!(context.scope.remaining.messages, 1);
            assert_eq!(context.scope.remaining.agents, 1);
            assert_eq!(context.scrub_summary.secrets_replaced, 0);
            let finished: Vec<_> = events
                .iter()
                .filter(|event| event.state == FullArchiveProgressState::Finished)
                .map(|event| event.phase)
                .collect();
            assert_eq!(
                finished,
                [
                    "snapshot_source_open",
                    "snapshot_full_copy",
                    "snapshot_neutralize",
                    "snapshot_validate",
                    "snapshot_publish"
                ]
            );
            let copied = private_source(&destination);
            let row = &copied
                .query_sync("SELECT registration_token, operator_note FROM agents", &[])
                .unwrap()[0];
            assert_eq!(
                row.get_named::<String>("registration_token").unwrap(),
                "registration-secret"
            );
            assert_eq!(
                row.get_named::<String>("operator_note").unwrap(),
                "extra authoritative column"
            );
            assert_eq!(
                copied
                    .query_sync("SELECT generation FROM db_identity", &[])
                    .unwrap()[0]
                    .get_named::<String>("generation")
                    .unwrap(),
                "durable-generation"
            );
            assert_eq!(
                copied
                    .query_sync("SELECT hex(payload) AS payload FROM future_state", &[])
                    .unwrap()[0]
                    .get_named::<String>("payload")
                    .unwrap(),
                "00FF"
            );
            assert_eq!(
                copied.query_sync("PRAGMA user_version", &[]).unwrap()[0]
                    .get_named::<i64>("user_version")
                    .unwrap(),
                231
            );
            assert_eq!(
                copied.query_sync("PRAGMA application_id", &[]).unwrap()[0]
                    .get_named::<i64>("application_id")
                    .unwrap(),
                4242
            );
            assert_eq!(
                copied
                    .query_sync("SELECT COUNT(*) AS n FROM backup_audit", &[])
                    .unwrap()[0]
                    .get_named::<i64>("n")
                    .unwrap(),
                1,
                "copying must not fire user triggers"
            );
            assert_eq!(copied.query_sync("SELECT COUNT(*) AS n FROM sqlite_master WHERE name='future_payload' AND type='index'", &[]).unwrap()[0].get_named::<i64>("n").unwrap(), 1);
            copied
                .execute_raw("INSERT INTO future_state(name,payload) VALUES('two',X'01')")
                .unwrap();
            assert_eq!(
                copied
                    .query_sync("SELECT id FROM future_state WHERE name='two'", &[])
                    .unwrap()[0]
                    .get_named::<i64>("id")
                    .unwrap(),
                501
            );
            assert_eq!(
                copied
                    .query_sync("SELECT COUNT(*) AS n FROM backup_audit", &[])
                    .unwrap()[0]
                    .get_named::<i64>("n")
                    .unwrap(),
                2,
                "restored triggers must still work"
            );
            copied
                .execute_raw("INSERT INTO future_state(name,payload) VALUES('two',X'02')")
                .expect_err("restored unique constraint must still apply");
        }
    }

    #[test]
    fn full_archive_reads_committed_wal_without_mutating_source_family() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("wal-archive.sqlite3");
        let destination = directory.path().join("wal-copy.sqlite3");
        let writer = DbConn::open_file(source.display().to_string()).unwrap();
        writer
            .execute_raw("CREATE TABLE backup_witness(id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        writer.execute_raw("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO backup_witness VALUES(1,'committed WAL payload')").unwrap();
        drop(writer);
        let family = || {
            [
                "",
                "-wal",
                "-shm",
                "-fsqlite-ns-gate",
                "-fsqlite-ns-use",
                "-wal-cert",
                "-wal-cert-head",
                "-journal",
            ]
            .map(|suffix| std::fs::read(format!("{}{suffix}", source.display())).ok())
        };
        let before = family();
        assert!(before[1].as_ref().is_some_and(|wal| wal.len() > 32));
        create_full_archive_snapshot_with_progress(&source, &destination, &mut |_| Ok(())).unwrap();
        assert_eq!(
            family(),
            before,
            "backup must not checkpoint or rewrite the source family"
        );
        let copied = private_source(&destination);
        assert_eq!(
            copied
                .query_sync("SELECT value FROM backup_witness", &[])
                .unwrap()[0]
                .get_named::<String>("value")
                .unwrap(),
            "committed WAL payload"
        );
        assert!(!sqlite_sidecar_artifacts_exist(&destination).unwrap());
    }

    #[test]
    fn full_archive_failed_publication_preserves_previous_copy_and_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("publication-source.sqlite3");
        let destination = directory.path().join("publication-copy.sqlite3");
        seed_reply_metadata_source(&private_source(&source), false);
        let source_before = std::fs::read(&source).unwrap();
        let mut events = Vec::new();
        let error = create_private_canonical_full_archive_snapshot_with_progress(
            &source,
            &destination,
            &mut |event| {
                if event.phase == "snapshot_publish"
                    && event.state == FullArchiveProgressState::Started
                {
                    std::fs::write(&destination, b"previous backup evidence").unwrap();
                }
                events.push(event);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShareError::SnapshotDestinationExists { .. }
        ));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"previous backup evidence"
        );
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        assert_eq!(events.last().unwrap().phase, "snapshot_publish");
        assert_eq!(
            events.last().unwrap().state,
            FullArchiveProgressState::Failed
        );
        let retained = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".archive-snapshot-stage.")
            })
            .expect("publication collision must retain the completed private image");
        assert_eq!(
            reply_metadata(&private_source(&retained.join("mailbox.sqlite3"))),
            reply_metadata(&private_source(&source))
        );
        let retry = directory.path().join("retry-copy.sqlite3");
        create_private_canonical_full_archive_snapshot_with_progress(&source, &retry, &mut |_| {
            Ok(())
        })
        .unwrap();
        assert_eq!(
            reply_metadata(&private_source(&retry)),
            reply_metadata(&private_source(&source))
        );
    }

    /// Run the incumbent projected archive and the full-image path together on
    /// identical input. This is opt-in because the incumbent can be expensive;
    /// the test asserts exact payload equality, never a host-dependent timing.
    #[test]
    #[ignore = "manual same-process archive scaling comparison"]
    fn archive_snapshot_scale_comparison() {
        let messages = std::env::var("AM_ARCHIVE_BENCH_MESSAGES")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(2000);
        assert!((7..=1_000_000).contains(&messages));
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("scale-source.sqlite3");
        let conn = private_source(&source);
        seed_reply_metadata_source(&conn, false);
        conn.execute_raw("INSERT INTO agents(id,project_id,name) VALUES(2,1,'RedFox'),(3,1,'GoldStone'); CREATE TABLE message_recipients(message_id INTEGER,agent_id INTEGER,kind TEXT,read_ts INTEGER,ack_ts INTEGER,PRIMARY KEY(message_id,agent_id,kind))").unwrap();
        conn.execute_sync(
            "WITH RECURSIVE ids(id) AS (VALUES(7) UNION ALL SELECT id+1 FROM ids WHERE id<?1) INSERT INTO messages(id,project_id,sender_id,subject,body_md,archive_metadata_json) SELECT id,1,1,printf('subject-%d',id),printf('message-%d: %s',id,?2),'{\"reply_to\":1}' FROM ids",
            &[Value::BigInt(messages), Value::Text("synthetic backup durability payload ".repeat(24))],
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients SELECT messages.id,agents.id,CASE agents.id WHEN 1 THEN 'to' WHEN 2 THEN 'cc' ELSE 'bcc' END,100,200 FROM messages CROSS JOIN agents").unwrap();
        drop(conn);
        // Admit a closed copy of exactly the same private fixture. Canonical
        // reads below always use `source`; they never open the runtime inode.
        let runtime_source = directory.path().join("scale-runtime-source.sqlite3");
        std::fs::copy(&source, &runtime_source).unwrap();
        drop(DbConn::open_file(runtime_source.display().to_string()).unwrap());
        for live in [false, true] {
            let engine = if live { "runtime" } else { "canonical" };
            let selected_source = if live { &runtime_source } else { &source };
            let projected = directory
                .path()
                .join(format!("scale-{engine}-projected.sqlite3"));
            let full = directory
                .path()
                .join(format!("scale-{engine}-full.sqlite3"));
            let started = Instant::now();
            if live {
                create_snapshot_context(
                    selected_source,
                    &projected,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                )
            } else {
                create_private_canonical_snapshot_context(
                    selected_source,
                    &projected,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                )
            }
            .unwrap();
            let projected_ms = started.elapsed().as_millis();
            let started = Instant::now();
            let mut phases = Vec::new();
            let mut progress = |event: FullArchiveProgress| {
                if event.state == FullArchiveProgressState::Finished {
                    phases.push((event.phase, event.elapsed.as_millis()));
                }
                Ok(())
            };
            if live {
                create_full_archive_snapshot_with_progress(selected_source, &full, &mut progress)
            } else {
                create_private_canonical_full_archive_snapshot_with_progress(
                    selected_source,
                    &full,
                    &mut progress,
                )
            }
            .unwrap();
            let full_ms = started.elapsed().as_millis();
            for (sql, columns) in [
                (
                    "SELECT id,body_md,subject,recipients_json,archive_metadata_json FROM messages ORDER BY id",
                    [
                        "id",
                        "body_md",
                        "subject",
                        "recipients_json",
                        "archive_metadata_json",
                    ],
                ),
                (
                    "SELECT message_id,agent_id,kind,CAST(read_ts AS TEXT) AS read_ts,CAST(ack_ts AS TEXT) AS ack_ts FROM message_recipients ORDER BY message_id,agent_id,kind",
                    ["message_id", "agent_id", "kind", "read_ts", "ack_ts"],
                ),
            ] {
                let expected = private_source(&source).query_sync(sql, &[]).unwrap();
                let actual = private_source(&full).query_sync(sql, &[]).unwrap();
                let incumbent = private_source(&projected).query_sync(sql, &[]).unwrap();
                assert_eq!(actual.len(), expected.len());
                assert_eq!(incumbent.len(), expected.len());
                for ((expected, actual), incumbent) in expected.iter().zip(&actual).zip(&incumbent)
                {
                    for column in columns {
                        assert_eq!(actual.get_by_name(column), expected.get_by_name(column));
                        assert_eq!(incumbent.get_by_name(column), expected.get_by_name(column));
                    }
                }
            }
            eprintln!(
                "{}",
                serde_json::json!({
                    "fixture": "same_process_private_archive",
                    "source_engine": engine,
                    "ordering": "projected_then_full",
                    "messages": messages,
                    "recipients": messages*3,
                    "projected_ms": projected_ms,
                    "full_ms": full_ms,
                    "full_phase_ms": phases,
                    "projected_bytes": std::fs::metadata(&projected).unwrap().len(),
                    "full_bytes": std::fs::metadata(&full).unwrap().len(),
                    "payload_equal": true,
                    "production_bottleneck_attributed": false,
                })
            );
        }
    }

    #[test]
    fn full_archive_cancellation_stops_before_publication_without_source_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cancel-source.sqlite3");
        seed_reply_metadata_source(&private_source(&source), false);
        let source_before = std::fs::read(&source).unwrap();
        for (index, (phase, state)) in [
            ("snapshot_source_open", FullArchiveProgressState::Started),
            ("snapshot_full_copy", FullArchiveProgressState::Started),
            ("snapshot_full_copy", FullArchiveProgressState::Finished),
            ("snapshot_neutralize", FullArchiveProgressState::Started),
            ("snapshot_validate", FullArchiveProgressState::Started),
            ("snapshot_validate", FullArchiveProgressState::Finished),
            ("snapshot_publish", FullArchiveProgressState::Started),
        ]
        .into_iter()
        .enumerate()
        {
            let destination = directory.path().join(format!("cancel-{index}.sqlite3"));
            let mut events = Vec::new();
            let error = create_private_canonical_full_archive_snapshot_with_progress(
                &source,
                &destination,
                &mut |event| {
                    events.push(event);
                    if event.phase == phase && event.state == state {
                        Err(ShareError::Validation {
                            message: "stop native archive at checkpoint".to_string(),
                        })
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("stop native archive at checkpoint")
            );
            assert!(
                !destination.exists(),
                "cancellation must not publish an image"
            );
            assert_eq!(std::fs::read(&source).unwrap(), source_before);
            let last = events.last().unwrap();
            assert_eq!((last.phase, last.state), (phase, state));
        }
    }

    fn private_source(path: &Path) -> CanonicalDbConn {
        CanonicalDbConn::open_file(path.to_str().unwrap()).unwrap()
    }

    fn seed_reply_metadata_source(conn: &impl SnapshotSource, legacy: bool) {
        for sql in [
            "CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            "CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT DEFAULT 'test', model TEXT DEFAULT 'test', task_description TEXT DEFAULT '', inception_ts INTEGER DEFAULT 0, last_active_ts INTEGER DEFAULT 0, attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, thread_id TEXT DEFAULT 'thread', topic TEXT DEFAULT 'handoff', subject TEXT DEFAULT 'subject', body_md TEXT DEFAULT 'body', importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, created_ts INTEGER DEFAULT 1707000000000000, recipients_json TEXT DEFAULT '{}', attachments TEXT DEFAULT '[]')",
        ] {
            conn.execute_snapshot(sql).unwrap();
        }
        // Add the optional column before any rows: this fixture is not a v30
        // short-record migration, which has its own materialization coverage.
        if !legacy {
            conn.execute_snapshot("ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT")
                .unwrap();
        }
        for sql in [
            "INSERT INTO projects VALUES(1,'reply-copy','/reply-copy',1707000000000000)",
            "INSERT INTO agents(id,project_id,name) VALUES(1,1,'BlueLake')",
            "INSERT INTO messages(id,project_id,sender_id) VALUES(1,1,1),(2,1,1),(3,1,1),(4,1,1),(5,1,1),(6,1,1)",
        ] {
            conn.execute_snapshot(sql).unwrap();
        }
        if !legacy {
            for sql in [
                "UPDATE messages SET archive_metadata_json='{}' WHERE id=2",
                "UPDATE messages SET archive_metadata_json='{\"reply_to\":2}' WHERE id=3",
                "UPDATE messages SET archive_metadata_json=' { \"reply_to\" : 2 } ' WHERE id=4",
                "UPDATE messages SET archive_metadata_json='{\"future\":\"private recovery payload\",\"reply_to\":2}' WHERE id=5",
                "UPDATE messages SET archive_metadata_json='not-json' WHERE id=6",
            ] {
                conn.execute_snapshot(sql).unwrap();
            }
        }
    }

    fn reply_metadata(conn: &impl SnapshotSource) -> Vec<(i64, Option<String>)> {
        conn.query_snapshot(
            "SELECT id, archive_metadata_json FROM messages ORDER BY id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get_named("id").unwrap(),
                row.get_named("archive_metadata_json").unwrap(),
            )
        })
        .collect()
    }

    const SNAPSHOT_SUCCESS_EVENTS: [SnapshotPhaseEvent; 8] = [
        SnapshotPhaseEvent::Started(SnapshotPhase::Snapshot),
        SnapshotPhaseEvent::Finished(SnapshotPhase::Snapshot),
        SnapshotPhaseEvent::Started(SnapshotPhase::Filter),
        SnapshotPhaseEvent::Finished(SnapshotPhase::Filter),
        SnapshotPhaseEvent::Started(SnapshotPhase::Scrub),
        SnapshotPhaseEvent::Finished(SnapshotPhase::Scrub),
        SnapshotPhaseEvent::Started(SnapshotPhase::Finalize),
        SnapshotPhaseEvent::Finished(SnapshotPhase::Finalize),
    ];

    #[test]
    fn snapshot_observer_reports_real_pipeline_and_preserves_private_metadata() {
        for live in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.sqlite3");
            let snapshot = dir.path().join("snapshot.sqlite3");
            let original = if live {
                let conn = DbConn::open_file(source.display().to_string()).unwrap();
                seed_reply_metadata_source(&conn, false);
                reply_metadata(&conn)
            } else {
                let conn = private_source(&source);
                seed_reply_metadata_source(&conn, false);
                reply_metadata(&conn)
            };
            let mut events = Vec::new();
            let context = if live {
                create_snapshot_context_with_observer(
                    &source,
                    &snapshot,
                    &["reply-copy".to_string()],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                    &mut |event| events.push(event),
                )
            } else {
                create_private_canonical_snapshot_context_with_observer(
                    &source,
                    &snapshot,
                    &["reply-copy".to_string()],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                    &mut |event| events.push(event),
                )
            }
            .unwrap();
            assert_eq!(events, SNAPSHOT_SUCCESS_EVENTS);
            assert_eq!(context.scope.projects.len(), 1);
            assert_eq!(context.scope.remaining.messages, 6);
            assert_eq!(reply_metadata(&private_source(&snapshot)), original);
            let unchanged = if live {
                reply_metadata(&DbConn::open_file(source.display().to_string()).unwrap())
            } else {
                reply_metadata(&private_source(&source))
            };
            assert_eq!(
                unchanged, original,
                "observation must not mutate the source"
            );
        }
    }

    #[test]
    fn snapshot_observer_does_not_finish_or_advance_after_snapshot_failure() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sqlite3");
        let snapshot = dir.path().join("snapshot.sqlite3");
        for live in [false, true] {
            let mut events = Vec::new();
            let result = if live {
                create_snapshot_context_with_observer(
                    &missing,
                    &snapshot,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                    &mut |event| events.push(event),
                )
            } else {
                create_private_canonical_snapshot_context_with_observer(
                    &missing,
                    &snapshot,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                    &mut |event| events.push(event),
                )
            };
            assert!(matches!(
                result,
                Err(ShareError::SnapshotSourceNotFound { .. })
            ));
            assert_eq!(events, SNAPSHOT_SUCCESS_EVENTS[..1]);
            assert!(!snapshot.exists());
            assert!(!missing.exists());
        }
    }

    #[test]
    fn snapshot_observer_attributes_unknown_project_to_filter_phase() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let snapshot = dir.path().join("snapshot.sqlite3");
        seed_reply_metadata_source(&private_source(&source), false);
        let original = reply_metadata(&private_source(&source));
        let mut events = Vec::new();
        let error = create_private_canonical_snapshot_context_with_observer(
            &source,
            &snapshot,
            &["unknown-private-project".to_string()],
            crate::ScrubPreset::Archive,
            SnapshotPurpose::PrivateArchive,
            &mut |event| events.push(event),
        )
        .unwrap_err();
        assert!(matches!(error, ShareError::ScopeIdentifierNotFound { .. }));
        assert_eq!(events, SNAPSHOT_SUCCESS_EVENTS[..3]);
        assert_eq!(reply_metadata(&private_source(&source)), original);
    }

    #[test]
    fn snapshot_observer_stops_at_failing_scrub_or_finalize_phase() {
        for (phase, expected_events) in [
            (SnapshotPhase::Scrub, &SNAPSHOT_SUCCESS_EVENTS[..5]),
            (SnapshotPhase::Finalize, &SNAPSHOT_SUCCESS_EVENTS[..7]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.sqlite3");
            let snapshot = dir.path().join("snapshot.sqlite3");
            let preserved = dir.path().join("snapshot-before-failure.sqlite3");
            seed_reply_metadata_source(&private_source(&source), false);
            let original = reply_metadata(&private_source(&source));
            let mut events = Vec::new();
            let error = create_private_canonical_snapshot_context_with_observer(
                &source,
                &snapshot,
                &[],
                crate::ScrubPreset::Archive,
                SnapshotPurpose::PrivateArchive,
                &mut |event| {
                    events.push(event);
                    if event == SnapshotPhaseEvent::Started(phase) {
                        // Disposable-fixture fault: preserve the copy, then
                        // make the next real phase encounter a non-file path.
                        std::fs::rename(&snapshot, &preserved).unwrap();
                        std::fs::create_dir(&snapshot).unwrap();
                    }
                },
            )
            .unwrap_err();
            assert!(matches!(error, ShareError::Validation { .. }));
            assert_eq!(events, expected_events);
            assert!(preserved.is_file(), "keep the failure evidence intact");
            assert_eq!(reply_metadata(&private_source(&source)), original);
        }
    }

    #[test]
    fn snapshot_preserves_exact_reply_authority_through_live_and_private_copies() {
        for live in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("metadata-source.sqlite3");
            let copy = dir.path().join("metadata-copy.sqlite3");
            let rebuilt = dir.path().join("metadata-rebuilt.sqlite3");
            let expected = if live {
                let conn = DbConn::open_file(source.display().to_string()).unwrap();
                seed_reply_metadata_source(&conn, false);
                reply_metadata(&conn)
            } else {
                let conn = private_source(&source);
                seed_reply_metadata_source(&conn, false);
                reply_metadata(&conn)
            };
            assert_eq!(expected[0], (1, None));
            assert_eq!(expected[1], (2, Some("{}".to_string())));
            assert_eq!(expected[2], (3, Some("{\"reply_to\":2}".to_string())));
            if live {
                create_sqlite_snapshot(&source, &copy, false).unwrap();
            } else {
                create_private_canonical_sqlite_snapshot(&source, &copy, false).unwrap();
            }
            assert_eq!(reply_metadata(&private_source(&copy)), expected);
            create_private_canonical_sqlite_snapshot(&copy, &rebuilt, false).unwrap();
            assert_eq!(reply_metadata(&private_source(&rebuilt)), expected);
            let unchanged = if live {
                reply_metadata(&DbConn::open_file(source.display().to_string()).unwrap())
            } else {
                reply_metadata(&private_source(&source))
            };
            assert_eq!(unchanged, expected, "source must remain unchanged");
        }
    }

    #[test]
    fn legacy_snapshot_metadata_remains_unknown_instead_of_becoming_a_send() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy-metadata.sqlite3");
        let copy = dir.path().join("legacy-copy.sqlite3");
        let rebuilt = dir.path().join("legacy-rebuilt.sqlite3");
        seed_reply_metadata_source(&private_source(&source), true);
        create_private_canonical_sqlite_snapshot(&source, &copy, false).unwrap();
        create_private_canonical_sqlite_snapshot(&copy, &rebuilt, false).unwrap();
        let expected: Vec<_> = (1..=6).map(|id| (id, None)).collect();
        assert_eq!(reply_metadata(&private_source(&copy)), expected);
        assert_eq!(reply_metadata(&private_source(&rebuilt)), expected);
    }

    #[test]
    fn snapshot_context_preserves_private_metadata_and_clears_every_publish_preset() {
        for preset in [
            crate::ScrubPreset::Standard,
            crate::ScrubPreset::Strict,
            crate::ScrubPreset::Archive,
        ] {
            for purpose in [SnapshotPurpose::Publish, SnapshotPurpose::PrivateArchive] {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("context-metadata.sqlite3");
                let snapshot = dir.path().join("context-copy.sqlite3");
                seed_reply_metadata_source(&private_source(&source), false);
                let original = reply_metadata(&private_source(&source));
                let context = create_private_canonical_snapshot_context(
                    &source,
                    &snapshot,
                    &[],
                    preset,
                    purpose,
                )
                .unwrap();
                let expected = if purpose == SnapshotPurpose::PrivateArchive
                    && matches!(preset, crate::ScrubPreset::Archive)
                {
                    original.clone()
                } else {
                    original.iter().map(|(id, _)| (*id, None)).collect()
                };
                assert_eq!(reply_metadata(&private_source(&snapshot)), expected);
                assert_eq!(reply_metadata(&private_source(&source)), original);
                let copied = private_source(&snapshot);
                let derived = copied
                    .query_sync(
                        "SELECT name FROM sqlite_master WHERE name IN \
                         ('fts_messages', 'message_overview_mv', 'attachments_by_message_mv', \
                          'fts_search_overview_mv') ORDER BY name",
                        &[],
                    )
                    .unwrap();
                if purpose == SnapshotPurpose::PrivateArchive {
                    assert!(!context.fts_enabled);
                    assert!(derived.is_empty(), "backups do not need viewer artifacts");
                } else {
                    assert!(context.fts_enabled);
                    assert_eq!(derived.len(), 4, "published bundles keep their viewer data");
                    let overview = copied
                        .query_sync("SELECT id FROM message_overview_mv ORDER BY id", &[])
                        .unwrap();
                    assert_eq!(overview.len(), original.len());
                }
                let bytes = std::fs::read(&snapshot).unwrap();
                let private_payload = b"private recovery payload";
                assert_eq!(
                    bytes
                        .windows(private_payload.len())
                        .any(|part| part == private_payload),
                    purpose == SnapshotPurpose::PrivateArchive
                        && matches!(preset, crate::ScrubPreset::Archive),
                    "compaction must remove scrubbed payload bytes for {purpose:?}/{preset:?}",
                );
            }
        }
    }

    #[test]
    fn private_archive_context_keeps_restore_authority_without_viewer_work() {
        fn seed(conn: &impl SnapshotSource) {
            for sql in [
                "CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
                "CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, reaper_exempt INTEGER, registration_token TEXT, retired_at INTEGER)",
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, body_md TEXT, ack_required INTEGER, recipients_json TEXT, attachments TEXT, archive_metadata_json TEXT)",
                "CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id,kind))",
                "CREATE TABLE file_reservations(id INTEGER PRIMARY KEY, project_id INTEGER, agent_id INTEGER, path_pattern TEXT, released_ts INTEGER)",
                "CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)",
                "CREATE TABLE agent_deregistrations(agent_id INTEGER PRIMARY KEY, deregistered_at INTEGER)",
                "CREATE TABLE project_mailbox_deliveries(message_id INTEGER PRIMARY KEY, project_id INTEGER, kind TEXT, delivered_ts INTEGER)",
                "CREATE TABLE project_mailbox_receipts(message_id INTEGER, agent_id INTEGER, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id))",
                "INSERT INTO projects VALUES(1,'restore-state','/restore-state',1707000000000000)",
                "INSERT INTO agents VALUES(-9,1,'RetiredLake',1,'retired-secret',800),(0,1,'BlueLake',1,'blue-secret',NULL),(1,1,'RedLake',0,'red-secret',NULL)",
                r#"INSERT INTO messages VALUES(-7,1,1,'private body',1,'{"to":["BlueLake"],"cc":["BlueLake"],"bcc":[]}','[{"path":"private.txt"}]',' { "reply_to" : 0 } '),(0,1,1,'zero identity',0,'{}','[]','{}'),(1,1,1,'shared body',1,'{"to":["project:restore-state"],"cc":[],"bcc":[]}','[]',NULL)"#,
                "INSERT INTO message_recipients VALUES(-7,0,'cc',NULL,222),(-7,0,'to',111,NULL),(0,1,'to',333,444)",
                "INSERT INTO file_reservations VALUES(-3,1,0,'src/**',900)",
                "INSERT INTO file_reservation_releases VALUES(-3,900)",
                "INSERT INTO agent_deregistrations VALUES(-9,800)",
                "INSERT INTO project_mailbox_deliveries VALUES(1,1,'to',500)",
                "INSERT INTO project_mailbox_receipts VALUES(1,0,100,110)",
            ] {
                conn.execute_snapshot(sql).unwrap();
            }
        }

        fn restore_state(conn: &impl SnapshotSource) -> Vec<Vec<Value>> {
            let mut result = Vec::new();
            for (sql, columns) in [
                (
                    "SELECT id,slug,human_key,CAST(created_at AS TEXT) AS created_at FROM projects ORDER BY id",
                    &["id", "slug", "human_key", "created_at"][..],
                ),
                (
                    "SELECT id,name,reaper_exempt,registration_token,CAST(retired_at AS TEXT) AS retired_at FROM agents ORDER BY id",
                    &[
                        "id",
                        "name",
                        "reaper_exempt",
                        "registration_token",
                        "retired_at",
                    ][..],
                ),
                (
                    "SELECT id,body_md,ack_required,recipients_json,attachments,archive_metadata_json FROM messages ORDER BY id",
                    &[
                        "id",
                        "body_md",
                        "ack_required",
                        "recipients_json",
                        "attachments",
                        "archive_metadata_json",
                    ][..],
                ),
                (
                    "SELECT message_id,agent_id,kind,CAST(read_ts AS TEXT) AS read_ts,CAST(ack_ts AS TEXT) AS ack_ts FROM message_recipients ORDER BY message_id,agent_id,kind",
                    &["message_id", "agent_id", "kind", "read_ts", "ack_ts"][..],
                ),
                (
                    "SELECT id,path_pattern,CAST(released_ts AS TEXT) AS released_ts FROM file_reservations ORDER BY id",
                    &["id", "path_pattern", "released_ts"][..],
                ),
                (
                    "SELECT reservation_id,CAST(released_ts AS TEXT) AS released_ts FROM file_reservation_releases ORDER BY reservation_id",
                    &["reservation_id", "released_ts"][..],
                ),
                (
                    "SELECT agent_id,CAST(deregistered_at AS TEXT) AS deregistered_at FROM agent_deregistrations ORDER BY agent_id",
                    &["agent_id", "deregistered_at"][..],
                ),
                (
                    "SELECT message_id,project_id,kind,delivered_ts FROM project_mailbox_deliveries ORDER BY message_id",
                    &["message_id", "project_id", "kind", "delivered_ts"][..],
                ),
                (
                    "SELECT message_id,agent_id,read_ts,ack_ts FROM project_mailbox_receipts ORDER BY message_id,agent_id",
                    &["message_id", "agent_id", "read_ts", "ack_ts"][..],
                ),
            ] {
                for row in conn.query_snapshot(sql, &[]).unwrap() {
                    result.push(
                        columns
                            .iter()
                            .map(|column| match row.get_by_name(column).unwrap() {
                                Value::Int(value) => Value::BigInt(i64::from(*value)),
                                value => value.clone(),
                            })
                            .collect(),
                    );
                }
            }
            result
        }

        for live in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.sqlite3");
            let snapshot = directory.path().join("archive.sqlite3");
            let original = if live {
                let conn = DbConn::open_file(source.to_str().unwrap()).unwrap();
                seed(&conn);
                restore_state(&conn)
            } else {
                let conn = private_source(&source);
                seed(&conn);
                restore_state(&conn)
            };
            let context = if live {
                create_snapshot_context(
                    &source,
                    &snapshot,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                )
            } else {
                create_private_canonical_snapshot_context(
                    &source,
                    &snapshot,
                    &[],
                    crate::ScrubPreset::Archive,
                    SnapshotPurpose::PrivateArchive,
                )
            }
            .unwrap();
            assert!(!context.fts_enabled);
            assert_eq!(context.scope.remaining.messages, 3);
            assert!(!sqlite_sidecar_artifacts_exist(&snapshot).unwrap());
            let copied = private_source(&snapshot);
            assert_eq!(restore_state(&copied), original);
            let integrity = copied.query_sync("PRAGMA integrity_check", &[]).unwrap();
            assert_eq!(integrity.len(), 1);
            assert_eq!(integrity[0].get_as::<String>(0).unwrap(), "ok");
            let unchanged = if live {
                restore_state(&DbConn::open_file(source.to_str().unwrap()).unwrap())
            } else {
                restore_state(&private_source(&source))
            };
            assert_eq!(unchanged, original, "the source remains authoritative");
        }
    }

    fn seed_large_private_archive(path: &Path) -> usize {
        let conn = private_source(path);
        seed_reply_metadata_source(&conn, false);
        conn.execute_raw(
            "ALTER TABLE agents ADD COLUMN registration_token TEXT; \
             UPDATE agents SET registration_token='restorable-registration-token'; \
             CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, \
                 read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id)); \
             BEGIN IMMEDIATE",
        )
        .unwrap();
        let count = 2 * SNAPSHOT_PAGE_ROWS + 7;
        for id in 1..=i64::try_from(count).unwrap() {
            if id > 6 {
                conn.execute_sync(
                    "INSERT INTO messages(id,project_id,sender_id,body_md,attachments,archive_metadata_json) \
                     VALUES(?1,1,1,?2,?3,?4)",
                    &[
                        Value::BigInt(id),
                        Value::Text(format!("message body {id}")),
                        Value::Text(format!("[{{\"path\":\"attachments/item-{id}.bin\"}}]")),
                        Value::Text(format!("{{\"reply_to\":{}}}", id - 1)),
                    ],
                )
                .unwrap();
            }
            conn.execute_sync(
                "INSERT INTO message_recipients VALUES(?1,1,'to',?2,?3)",
                &[
                    Value::BigInt(id),
                    Value::BigInt(id * 2),
                    Value::BigInt(id * 3),
                ],
            )
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        count
    }

    #[test]
    fn private_archive_progress_bounds_copy_pages_without_building_viewer_data() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("large-source.sqlite3");
        let snapshot = dir.path().join("large-archive.sqlite3");
        let count = seed_large_private_archive(&source);
        let original = std::fs::read(&source).unwrap();
        let mut events = Vec::new();
        let context = create_private_canonical_snapshot_context_with_progress(
            &source,
            &snapshot,
            &[],
            crate::ScrubPreset::Archive,
            SnapshotPurpose::PrivateArchive,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap();
        assert!(!context.fts_enabled);
        let mut phases = events.iter().map(|event| event.phase).collect::<Vec<_>>();
        phases.dedup();
        assert_eq!(
            phases,
            [
                SnapshotProgressPhase::Copy,
                SnapshotProgressPhase::Scope,
                SnapshotProgressPhase::Scrub,
                SnapshotProgressPhase::Compact
            ]
        );
        for phase in [SnapshotProgressPhase::Copy, SnapshotProgressPhase::Compact] {
            for table in ["messages", "message_recipients"] {
                let counts = events
                    .iter()
                    .filter(|event| event.phase == phase && event.table == Some(table))
                    .map(|event| event.rows_copied)
                    .collect::<Vec<_>>();
                assert_eq!(counts, [0, 1000, 2000, 2007]);
            }
        }
        let conn = private_source(&snapshot);
        let rows = conn
            .query_sync(
                "SELECT id,body_md,attachments,archive_metadata_json FROM messages ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), count);
        for (index, row) in rows.iter().enumerate().skip(6) {
            let id = i64::try_from(index + 1).unwrap();
            assert_eq!(row.get_named::<i64>("id").unwrap(), id);
            assert_eq!(
                row.get_named::<String>("body_md").unwrap(),
                format!("message body {id}")
            );
            assert_eq!(
                row.get_named::<String>("attachments").unwrap(),
                format!("[{{\"path\":\"attachments/item-{id}.bin\"}}]"),
            );
            assert_eq!(
                row.get_named::<String>("archive_metadata_json").unwrap(),
                format!("{{\"reply_to\":{}}}", id - 1),
            );
        }
        let receipts = conn
            .query_sync(
                "SELECT message_id, read_ts, ack_ts FROM message_recipients ORDER BY message_id",
                &[],
            )
            .unwrap();
        assert_eq!(receipts.len(), count);
        for (index, row) in receipts.iter().enumerate() {
            let id = i64::try_from(index + 1).unwrap();
            assert_eq!(row.get_named::<i64>("message_id").unwrap(), id);
            assert_eq!(
                row.get_named::<String>("read_ts").unwrap(),
                (id * 2).to_string()
            );
            assert_eq!(
                row.get_named::<String>("ack_ts").unwrap(),
                (id * 3).to_string()
            );
        }
        assert_eq!(
            conn.query_sync("SELECT registration_token FROM agents WHERE id=1", &[])
                .unwrap()[0]
                .get_named::<String>("registration_token")
                .unwrap(),
            "restorable-registration-token",
        );
        assert!(
            conn.query_sync(
                "SELECT name FROM sqlite_master WHERE name LIKE 'fts_%' OR name LIKE '%_mv'",
                &[],
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            conn.query_sync("PRAGMA quick_check", &[]).unwrap()[0]
                .get_as::<String>(0)
                .unwrap(),
            "ok"
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }

    #[test]
    fn snapshot_cancel_after_copy_page_does_not_publish_and_releases_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cancel-source.sqlite3");
        let snapshot = dir.path().join("cancel-copy.sqlite3");
        seed_large_private_archive(&source);
        let original = std::fs::read(&source).unwrap();
        let mut stopped = false;
        let error = create_private_canonical_snapshot_context_with_progress(
            &source,
            &snapshot,
            &[],
            crate::ScrubPreset::Archive,
            SnapshotPurpose::PrivateArchive,
            &mut |event| {
                if event.phase == SnapshotProgressPhase::Copy
                    && event.table == Some("messages")
                    && event.rows_copied == 1000
                {
                    stopped = true;
                    return Err(ShareError::Io(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled by test observer",
                    )));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(stopped);
        assert!(
            matches!(error, ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::Interrupted)
        );
        assert!(!snapshot.exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        private_source(&source)
            .execute_raw("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
        // The same destination can be used after cancellation; no partial
        // image or transaction from the cancelled attempt is reusable authority.
        create_private_canonical_sqlite_snapshot(&source, &snapshot, false).unwrap();
        assert_eq!(
            private_source(&snapshot)
                .query_sync("SELECT count(*) AS n FROM messages", &[])
                .unwrap()[0]
                .get_named::<i64>("n")
                .unwrap(),
            2007,
        );
    }

    #[test]
    fn snapshot_cancel_during_compaction_preserves_the_prepared_image() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("compact-source.sqlite3");
        let snapshot = dir.path().join("compact-copy.sqlite3");
        seed_large_private_archive(&source);
        create_private_canonical_sqlite_snapshot(&source, &snapshot, false).unwrap();
        let before = std::fs::read(&snapshot).unwrap();
        let error = crate::finalize::finalize_snapshot_with_progress(&snapshot, &mut |event| {
            if event.table == Some("messages") && event.rows_copied == 1000 {
                return Err(ShareError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled while compacting",
                )));
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            matches!(error, ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::Interrupted)
        );
        assert_eq!(std::fs::read(&snapshot).unwrap(), before);
    }

    #[test]
    fn snapshot_observer_cancellation_rolls_back_the_source_read_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(&dir.path().join("held-source.sqlite3"));
        seed_reply_metadata_source(&source, false);
        let destination = private_source(&dir.path().join("held-copy.sqlite3"));
        let error = transfer_tables_with_progress(
            &source,
            &destination,
            SnapshotProgressPhase::Copy,
            &mut |event| {
                if event.table == Some("messages") && event.rows_copied > 0 {
                    return Err(ShareError::Io(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled source read",
                    )));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, ShareError::Io(ref error) if error.kind() == std::io::ErrorKind::Interrupted)
        );
        // Keep the source connection alive: Drop cannot mask a leaked read
        // transaction. A fresh transaction must work immediately after cancel.
        source.execute_raw("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(reply_metadata(&source).len(), 6);
    }

    #[test]
    #[ignore = "manual same-fixture private archive versus previous viewer-finalization path"]
    fn private_archive_and_viewer_baseline_performance() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("benchmark-source.sqlite3");
        let archive = dir.path().join("benchmark-archive.sqlite3");
        let baseline = dir.path().join("benchmark-viewer-baseline.sqlite3");
        let messages = seed_large_private_archive(&source);

        // The incumbent archive path in the same invocation: snapshot, scope,
        // archive scrub and the full static-viewer finalization. This preserves
        // private fields exactly; using Publish would clear credentials and
        // would not be an equivalent baseline.
        let started = std::time::Instant::now();
        create_private_canonical_sqlite_snapshot(&source, &baseline, true).unwrap();
        crate::apply_project_scope(&baseline, &[]).unwrap();
        crate::scrub_snapshot(&baseline, crate::ScrubPreset::Archive).unwrap();
        let old_finalize = crate::finalize_export_db(&baseline).unwrap();
        let baseline_elapsed = started.elapsed();

        let started = std::time::Instant::now();
        let context = create_private_canonical_snapshot_context(
            &source,
            &archive,
            &[],
            crate::ScrubPreset::Archive,
            SnapshotPurpose::PrivateArchive,
        )
        .unwrap();
        let archive_elapsed = started.elapsed();
        assert!(old_finalize.fts_enabled);
        assert!(!context.fts_enabled);
        let previous = private_source(&baseline);
        let current = private_source(&archive);
        for table in KNOWN_TABLES {
            let sql = format!(
                "SELECT {} FROM \"{}\" ORDER BY {}",
                quoted_column_list(table.columns),
                table.name,
                quoted_column_list(table.primary_key_columns),
            );
            let values = |connection: &CanonicalDbConn| {
                connection
                    .query_sync(&sql, &[])
                    .unwrap()
                    .iter()
                    .map(|row| {
                        table
                            .columns
                            .iter()
                            .map(|column| {
                                row.get_by_name(column)
                                    .cloned()
                                    .expect("known snapshot column")
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(values(&current), values(&previous), "{}", table.name);
        }
        eprintln!(
            "archive snapshot comparison: messages={messages} baseline_ms={} archive_ms={} baseline_bytes={} archive_bytes={}",
            baseline_elapsed.as_millis(),
            archive_elapsed.as_millis(),
            std::fs::metadata(&baseline).unwrap().len(),
            std::fs::metadata(&archive).unwrap().len(),
        );
        // No wall-time threshold: record both measurements; correctness and
        // elimination of unnecessary derivatives are deterministic assertions.
    }

    #[test]
    fn snapshot_metadata_pagination_observes_one_wal_generation() {
        use std::cell::Cell;
        struct MetadataRace<'a> {
            reader: &'a CanonicalDbConn,
            writer: &'a CanonicalDbConn,
            pages: Cell<usize>,
        }
        impl SnapshotSource for MetadataRace<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.reader.execute_raw(sql)
            }
            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.reader.query_sync(sql, params)?;
                if sql.contains("FROM \"messages\"") {
                    assert!(sql.contains("\"archive_metadata_json\" AS \"archive_metadata_json\""));
                    assert!(sql.ends_with("LIMIT 1000"));
                    assert!(rows.len() <= 1000);
                    self.pages.set(self.pages.get() + 1);
                    if self.pages.get() == 1 {
                        self.writer.execute_raw("BEGIN IMMEDIATE; UPDATE messages SET archive_metadata_json='{\"reply_to\":2}', body_md='later body' WHERE id=1001; COMMIT")?;
                    }
                }
                Ok(rows)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wal-metadata.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("PRAGMA journal_mode=WAL").unwrap();
        seed_reply_metadata_source(&conn, false);
        conn.execute_raw("BEGIN IMMEDIATE").unwrap();
        for id in 7..=1001 {
            conn.execute_sync(
                "INSERT INTO messages(id,project_id,sender_id,archive_metadata_json) VALUES(?1,1,1,'{\"reply_to\":1}')",
                &[Value::BigInt(id)],
            )
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        let expected = reply_metadata(&conn);
        let writer = private_source(&source);
        let target = private_source(&dir.path().join("wal-metadata-copy.sqlite3"));
        let race = MetadataRace {
            reader: &conn,
            writer: &writer,
            pages: Cell::new(0),
        };
        transfer_tables_in_consistent_read(&race, &target).unwrap();
        assert_eq!(race.pages.get(), 3);
        assert_eq!(reply_metadata(&target), expected);
        assert_eq!(
            reply_metadata(&conn).last().unwrap().1.as_deref(),
            Some("{\"reply_to\":2}")
        );
        let copied_body = target
            .query_sync("SELECT body_md FROM messages WHERE id=1001", &[])
            .unwrap();
        assert_eq!(
            copied_body[0].get_named::<String>("body_md").unwrap(),
            "body"
        );
    }

    #[test]
    fn snapshot_pagination_preserves_the_entire_signed_identity_domain() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("signed-source.sqlite3");
        let copied = dir.path().join("signed-copy.sqlite3");
        let rebuilt = dir.path().join("signed-rebuilt.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)").unwrap();
        let ids = [i64::MIN, -1001, -1, 0, 1, i64::MAX];
        for id in ids {
            conn.execute_sync(
                "INSERT INTO projects VALUES(?1, ?2, ?3, 0)",
                &[
                    Value::BigInt(id),
                    Value::Text(format!("p-{id}")),
                    Value::Text(format!("/p-{id}")),
                ],
            )
            .unwrap();
        }
        drop(conn);
        create_private_canonical_sqlite_snapshot(&source, &copied, false).unwrap();
        create_private_canonical_sqlite_snapshot(&copied, &rebuilt, false).unwrap();
        for path in [&source, &copied, &rebuilt] {
            let rows = private_source(path)
                .query_sync("SELECT id FROM projects ORDER BY id", &[])
                .unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row.get_named::<i64>("id").unwrap())
                    .collect::<Vec<_>>(),
                ids
            );
        }
    }

    #[test]
    fn snapshot_recipient_pages_preserve_one_messages_large_population() {
        use std::cell::Cell;
        struct Recorded<'a> {
            source: &'a CanonicalDbConn,
            pages: Cell<usize>,
        }
        impl SnapshotSource for Recorded<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.source.execute_raw(sql)
            }
            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.source.query_sync(sql, params)?;
                if sql.contains("FROM \"message_recipients\"") {
                    assert!(sql.ends_with("LIMIT 1000"), "{sql}");
                    assert!(rows.len() <= SNAPSHOT_PAGE_ROWS);
                    self.pages.set(self.pages.get() + 1);
                }
                Ok(rows)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(&dir.path().join("many-recipients.sqlite3"));
        let target = private_source(&dir.path().join("many-recipients-copy.sqlite3"));
        source.execute_raw("CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id, kind)); BEGIN").unwrap();
        for agent in -500..650 {
            for kind in ["to", "cc", "bcc"] {
                source
                    .execute_sync(
                        "INSERT INTO message_recipients VALUES(-1, ?1, ?2, ?3, NULL)",
                        &[
                            Value::BigInt(agent),
                            Value::Text(kind.into()),
                            Value::BigInt(agent + 1000),
                        ],
                    )
                    .unwrap();
            }
        }
        source.execute_raw("COMMIT").unwrap();
        let recorded = Recorded {
            source: &source,
            pages: Cell::new(0),
        };
        transfer_tables_in_consistent_read(&recorded, &target).unwrap();
        assert_eq!(recorded.pages.get(), 5);
        let rows = target.query_sync("SELECT message_id, agent_id, kind, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id, kind", &[]).unwrap();
        assert_eq!(rows.len(), 3450);
        for (row, (agent, kind)) in rows
            .iter()
            .zip((-500..650).flat_map(|agent| ["bcc", "cc", "to"].map(|kind| (agent, kind))))
        {
            assert_eq!(row.get_named::<i64>("message_id").unwrap(), -1);
            assert_eq!(row.get_named::<i64>("agent_id").unwrap(), agent);
            assert_eq!(row.get_named::<String>("kind").unwrap(), kind);
            assert_eq!(
                row.get_named::<String>("read_ts").unwrap(),
                (agent + 1000).to_string()
            );
            assert_eq!(row.get_named::<Option<i64>>("ack_ts").unwrap(), None);
        }
    }

    #[test]
    fn recipient_continuation_seeks_past_previous_message_populations() {
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(&dir.path().join("recipient-seek.sqlite3"));
        source
            .execute_raw(
                "CREATE TABLE message_recipients(message_id INTEGER NOT NULL, \
             agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, \
             ack_ts INTEGER, PRIMARY KEY(message_id,agent_id)); BEGIN IMMEDIATE",
            )
            .unwrap();
        for id in 1..=2007 {
            source
                .execute_sync(
                    "INSERT INTO message_recipients VALUES(?1,1,'to',?2,?3)",
                    &[
                        Value::BigInt(id),
                        Value::BigInt(id * 2),
                        Value::BigInt(id * 3),
                    ],
                )
                .unwrap();
        }
        source.execute_raw("COMMIT").unwrap();
        let table = KNOWN_TABLES
            .iter()
            .find(|table| table.name == "message_recipients")
            .unwrap();
        let (sql, params) = snapshot_page_query(
            table,
            table.columns,
            table.primary_key_columns,
            Some(&[
                SnapshotKey::Integer(1000),
                SnapshotKey::Integer(1),
                SnapshotKey::Text("to".into()),
            ]),
        );
        let plan = source
            .query_sync(&format!("EXPLAIN QUERY PLAN {sql}"), &params)
            .unwrap();
        assert!(
            plan.iter().any(|row| {
                let detail = row.get_named::<String>("detail").unwrap();
                detail.contains("SEARCH") && detail.contains("message_recipients")
            }),
            "continuation must seek into the real primary-key index: {plan:?}"
        );
        let rows = source.query_sync(&sql, &params).unwrap();
        let ids = rows
            .iter()
            .map(|row| row.get_named::<i64>("message_id").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, (1001..=2000).collect::<Vec<_>>());
    }

    #[test]
    fn snapshot_pagination_accepts_uppercase_imported_key_columns() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("uppercase-source.sqlite3");
        let target = dir.path().join("uppercase-copy.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("CREATE TABLE PROJECTS(ID INTEGER PRIMARY KEY, SLUG TEXT, HUMAN_KEY TEXT, CREATED_AT INTEGER); INSERT INTO PROJECTS VALUES(-7, 'upper', '/upper', 17)").unwrap();
        drop(conn);
        create_private_canonical_sqlite_snapshot(&source, &target, false).unwrap();
        let rows = private_source(&target)
            .query_sync("SELECT id, slug, created_at FROM projects", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), -7);
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "upper");
        assert_eq!(rows[0].get_named::<String>("created_at").unwrap(), "17");
    }

    #[test]
    fn snapshot_refuses_unconstrained_keys_before_boundary_duplicates_are_lost() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("duplicate-source.sqlite3");
        let target = dir.path().join("duplicate-copy.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("CREATE TABLE projects(id INTEGER, slug TEXT, human_key TEXT, created_at INTEGER); BEGIN").unwrap();
        for n in 1..=1001 {
            let id = n.min(1000);
            conn.execute_sync(
                "INSERT INTO projects VALUES(?1, ?2, '/duplicate', 0)",
                &[Value::BigInt(id), Value::Text(format!("original-{n}"))],
            )
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        drop(conn);
        let error = create_private_canonical_sqlite_snapshot(&source, &target, false).unwrap_err();
        assert!(
            error.to_string().contains("not uniquely constrained"),
            "{error}"
        );
        assert!(!target.exists());
        assert_eq!(
            private_source(&source)
                .query_sync("SELECT count(*) AS n FROM projects", &[])
                .unwrap()[0]
                .get_named::<i64>("n")
                .unwrap(),
            1001
        );
    }

    #[test]
    fn snapshot_refuses_null_composite_keys_instead_of_skipping_a_tail() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("null-recipient.sqlite3");
        let target = dir.path().join("null-recipient-copy.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id,kind)); INSERT INTO message_recipients VALUES(1, 2, NULL, 17, 19)").unwrap();
        drop(conn);
        let error = create_private_canonical_sqlite_snapshot(&source, &target, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("non-text pagination column message_recipients.kind")
        );
        assert!(!target.exists());
        assert_eq!(
            private_source(&source)
                .query_sync("SELECT ack_ts FROM message_recipients", &[])
                .unwrap()[0]
                .get_named::<i64>("ack_ts")
                .unwrap(),
            19
        );
    }

    #[test]
    fn snapshot_composite_cursor_uses_binary_order_even_on_nocase_imports() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("nocase-source.sqlite3");
        let target = dir.path().join("nocase-copy.sqlite3");
        let conn = private_source(&source);
        conn.execute_raw("CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT COLLATE NOCASE, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id,kind)); BEGIN").unwrap();
        let kinds = (0..1100)
            .map(|n| format!("{}-{n:04}", if n % 2 == 0 { "B" } else { "a" }))
            .collect::<Vec<_>>();
        for kind in &kinds {
            conn.execute_sync(
                "INSERT INTO message_recipients VALUES(1, 2, ?1, NULL, 19)",
                &[Value::Text(kind.clone())],
            )
            .unwrap();
        }
        conn.execute_raw("COMMIT").unwrap();
        drop(conn);
        create_private_canonical_sqlite_snapshot(&source, &target, false).unwrap();
        let rows = private_source(&target)
            .query_sync(
                "SELECT kind FROM message_recipients ORDER BY kind COLLATE BINARY",
                &[],
            )
            .unwrap();
        let mut expected = kinds;
        expected.sort();
        assert_eq!(
            rows.iter()
                .map(|row| row.get_named::<String>("kind").unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn snapshot_refuses_nonadvancing_source_pages_and_releases_its_read() {
        struct Reversed<'a>(&'a CanonicalDbConn);
        impl SnapshotSource for Reversed<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.0.execute_raw(sql)
            }
            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let mut rows = self.0.query_sync(sql, params)?;
                if sql.contains("FROM \"projects\"") {
                    rows.reverse();
                }
                Ok(rows)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(&dir.path().join("reversed-source.sqlite3"));
        let target = private_source(&dir.path().join("reversed-copy.sqlite3"));
        source.execute_raw("CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER); INSERT INTO projects VALUES(1,'one','/one',0),(2,'two','/two',0)").unwrap();
        let error = transfer_tables_in_consistent_read(&Reversed(&source), &target).unwrap_err();
        assert!(error.to_string().contains("cursor did not advance"));
        source.execute_raw("BEGIN; ROLLBACK").unwrap();
    }

    #[test]
    fn snapshot_recipient_paging_keeps_one_wal_snapshot_across_peer_commits() {
        use std::cell::Cell;
        struct Interleaved<'a> {
            source: &'a CanonicalDbConn,
            writer: &'a CanonicalDbConn,
            fired: Cell<bool>,
        }
        impl SnapshotSource for Interleaved<'_> {
            fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
                self.source.execute_raw(sql)
            }
            fn query_snapshot(
                &self,
                sql: &str,
                params: &[Value],
            ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
                let rows = self.source.query_sync(sql, params)?;
                if sql.contains("FROM \"message_recipients\"") && !self.fired.replace(true) {
                    self.writer.execute_raw("BEGIN IMMEDIATE; UPDATE message_recipients SET ack_ts = 999 WHERE agent_id = 2000; INSERT INTO message_recipients VALUES(1,9999,'to',NULL,777); COMMIT")?;
                }
                Ok(rows)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paged-wal.sqlite3");
        let source = private_source(&path);
        source.execute_raw("PRAGMA journal_mode=WAL; CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id,agent_id)); BEGIN").unwrap();
        for agent in 1..=2300 {
            source
                .execute_sync(
                    "INSERT INTO message_recipients VALUES(1,?1,'to',NULL,NULL)",
                    &[Value::BigInt(agent)],
                )
                .unwrap();
        }
        source.execute_raw("COMMIT").unwrap();
        let writer = private_source(&path);
        let target = private_source(&dir.path().join("paged-wal-copy.sqlite3"));
        let interleaved = Interleaved {
            source: &source,
            writer: &writer,
            fired: Cell::new(false),
        };
        transfer_tables_in_consistent_read(&interleaved, &target).unwrap();
        assert!(interleaved.fired.get());
        assert_eq!(
            target
                .query_sync("SELECT count(*) AS n FROM message_recipients", &[])
                .unwrap()[0]
                .get_named::<i64>("n")
                .unwrap(),
            2300
        );
        assert_eq!(
            target
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE agent_id=2000",
                    &[]
                )
                .unwrap()[0]
                .get_named::<Option<String>>("ack_ts")
                .unwrap(),
            None
        );
        assert_eq!(
            source
                .query_sync("SELECT count(*) AS n FROM message_recipients", &[])
                .unwrap()[0]
                .get_named::<i64>("n")
                .unwrap(),
            2301
        );
        assert_eq!(
            source
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE agent_id=2000",
                    &[]
                )
                .unwrap()[0]
                .get_named::<i64>("ack_ts")
                .unwrap(),
            999
        );
    }

    struct InterleavingSnapshotSource {
        inner: DbConn,
        writer_trigger: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
        writer_done: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl SnapshotSource for InterleavingSnapshotSource {
        fn execute_snapshot(&self, sql: &str) -> Result<(), sqlmodel_core::Error> {
            self.inner.execute_raw(sql)
        }

        fn query_snapshot(
            &self,
            sql: &str,
            params: &[Value],
        ) -> Result<Vec<sqlmodel_core::Row>, sqlmodel_core::Error> {
            let rows = self.inner.query_sync(sql, params)?;
            if sql.contains("FROM \"projects\"") {
                let trigger = self
                    .writer_trigger
                    .lock()
                    .expect("lock snapshot test writer trigger")
                    .take();
                if let Some(trigger) = trigger {
                    trigger.send(()).expect("release concurrent WAL writer");
                    self.writer_done
                        .lock()
                        .expect("lock snapshot test writer completion")
                        .recv()
                        .expect("concurrent WAL writer commits");
                }
            }
            Ok(rows)
        }
    }

    // GH#230: the firmlink allowance itself is a pure predicate over
    // synthetic paths (real firmlinks cannot be fabricated at `/` inside a
    // test sandbox); acceptance lives in
    // `mcp_agent_mail_core::disk::is_platform_temp_firmlink` tests. Here we
    // pin down the share-side traversal: a fixture-local `var ->
    // private/var` symlink is NOT a firmlink (its canonical target is not
    // exactly `/private/var`), so it must stay refused.
    #[cfg(unix)]
    #[test]
    fn create_real_directory_all_rejects_fixture_var_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let private_var = dir.path().join("private").join("var");
        std::fs::create_dir_all(&private_var).unwrap();
        let linked_var = dir.path().join("var");
        symlink(&private_var, &linked_var).unwrap();

        let err = create_real_directory_all(&linked_var.join("out"))
            .expect_err("fixture var symlink is not a platform firmlink");
        match err {
            RealDirError::Symlink(component) => assert_eq!(component, linked_var),
            _ => panic!("expected symlink refusal"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_real_directory_all_rejects_plain_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let linked = dir.path().join("linked");
        symlink(&target, &linked).unwrap();

        assert!(matches!(
            create_real_directory_all(&linked.join("child")),
            Err(RealDirError::Symlink(_))
        ));
    }

    #[test]
    fn create_real_directory_all_accepts_and_creates_real_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        create_real_directory_all(&nested).expect("plain real dirs are accepted");
        assert!(nested.is_dir());
        // Idempotent on existing real directories.
        create_real_directory_all(&nested).expect("existing real dirs stay accepted");
    }

    #[test]
    fn create_real_directory_all_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            create_real_directory_all(&dir.path().join("..").join("escape")),
            Err(RealDirError::ParentTraversal)
        ));
    }

    #[test]
    fn snapshot_source_not_found() {
        let result = create_sqlite_snapshot(
            Path::new("/nonexistent/db.sqlite3"),
            Path::new("/tmp/dest.sqlite3"),
            true,
        );
        assert!(matches!(
            result,
            Err(ShareError::SnapshotSourceNotFound { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_source_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_source = dir.path().join("real-source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        let conn = DbConn::open_file(real_source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);

        let linked_source = dir.path().join("linked-source.sqlite3");
        symlink(&real_source, &linked_source).unwrap();

        let err = create_sqlite_snapshot(&linked_source, &dest, false)
            .expect_err("symlinked sources must fail validation");
        assert!(matches!(err, ShareError::Validation { .. }));
        assert!(err.to_string().contains("real file"));
    }

    #[test]
    fn snapshot_creates_valid_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create a minimal source DB with FrankenSQLite (like runtime).
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'hello', '/test', 0)")
            .unwrap();
        drop(conn);

        // Snapshot it into a standalone SQLite bundle database.
        let result = create_sqlite_snapshot(&source, &dest, false);
        assert!(result.is_ok());
        assert!(dest.exists());

        // Verify data in the copied snapshot.
        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let name: String = rows[0].get_named("slug").unwrap();
        assert_eq!(name, "hello");

        // Verify integrity on the copied snapshot.
        let rows = copy_conn.query_sync("PRAGMA integrity_check", &[]).unwrap();
        let result: String = rows[0].get_named("integrity_check").unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn private_canonical_snapshot_is_explicit_and_live_api_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("private-canonical.sqlite3");
        let private_dest = dir.path().join("private-copy.sqlite3");
        let refused_live_dest = dir.path().join("refused-live-copy.sqlite3");

        let conn = CanonicalDbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'private', '/private', 0)")
            .unwrap();
        drop(conn);

        for suffix in ["-fsqlite-ns-gate", "-fsqlite-ns-use"] {
            assert!(
                !PathBuf::from(format!("{}{suffix}", source.display())).exists(),
                "canonical fixture must not accidentally gain FrankenSQLite namespace authority"
            );
        }

        create_private_canonical_sqlite_snapshot(&source, &private_dest, true)
            .expect("explicit private canonical snapshot succeeds");
        let copy = CanonicalDbConn::open_file(private_dest.display().to_string()).unwrap();
        let rows = copy
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "private");

        let error = create_sqlite_snapshot(&source, &refused_live_dest, false)
            .expect_err("live API must not fall back for a namespace-less source");
        assert!(
            error
                .to_string()
                .contains("complete pre-existing namespace sidecar pair"),
            "unexpected live-source refusal: {error}"
        );
        assert!(!refused_live_dest.exists());
    }

    #[test]
    fn private_canonical_api_rejects_franken_namespace_authority() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("live-franken.sqlite3");
        let destination = dir.path().join("unsafe-canonical-copy.sqlite3");
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);

        let error = create_private_canonical_sqlite_snapshot(&source, &destination, false)
            .expect_err("private canonical API must refuse Franken namespace authority");
        assert!(
            error
                .to_string()
                .contains("refuses FrankenSQLite namespace artifact"),
            "unexpected namespace refusal: {error}"
        );
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_canonical_api_rejects_hard_linked_main_inode() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("private-canonical.sqlite3");
        let alias = dir.path().join("private-canonical-alias.sqlite3");
        let destination = dir.path().join("unsafe-hardlink-copy.sqlite3");
        let conn = CanonicalDbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);
        std::fs::hard_link(&source, &alias).unwrap();

        let error = create_private_canonical_sqlite_snapshot(&source, &destination, false)
            .expect_err("private canonical API must refuse multiply-linked main inodes");
        assert!(
            error
                .to_string()
                .contains("must have exactly one hard link"),
            "unexpected hard-link refusal: {error}"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn snapshot_pins_one_primary_and_wal_state_across_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("live-wal.sqlite3");
        let destination = dir.path().join("consistent-copy.sqlite3");

        let seed = DbConn::open_file(source.display().to_string()).unwrap();
        seed.execute_raw("PRAGMA journal_mode = WAL").unwrap();
        seed.execute_raw("PRAGMA wal_autocheckpoint = 0").unwrap();
        seed.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        seed.execute_raw(
            "CREATE TABLE products (id INTEGER PRIMARY KEY, product_uid TEXT, name TEXT, created_at INTEGER)",
        )
        .unwrap();
        seed.execute_raw("INSERT INTO projects VALUES (1, 'before', '/before', 0)")
            .unwrap();
        drop(seed);

        let reader = mcp_agent_mail_db::pool::open_guarded_read_only_franken_existing_file(
            &source,
            "consistent snapshot test",
        )
        .unwrap();
        let destination_conn =
            CanonicalDbConn::open_file(destination.display().to_string()).unwrap();
        let (writer_trigger_tx, writer_trigger_rx) = std::sync::mpsc::sync_channel(0);
        let (writer_done_tx, writer_done_rx) = std::sync::mpsc::sync_channel(0);
        let writer_path = source;
        let writer = std::thread::spawn(move || {
            let conn = DbConn::open_file(writer_path.display().to_string()).unwrap();
            conn.execute_raw("PRAGMA wal_autocheckpoint = 0").unwrap();
            writer_trigger_rx.recv().unwrap();
            conn.execute_raw("INSERT INTO products VALUES (1, 'later', 'Later', 1)")
                .unwrap();
            writer_done_tx.send(()).unwrap();
        });
        let interleaving_reader = InterleavingSnapshotSource {
            inner: reader,
            writer_trigger: std::sync::Mutex::new(Some(writer_trigger_tx)),
            writer_done: std::sync::Mutex::new(writer_done_rx),
        };

        transfer_tables_in_consistent_read(&interleaving_reader, &destination_conn)
            .expect("snapshot transfer stays on one source read transaction");
        writer.join().unwrap();

        let projects = destination_conn
            .query_sync("SELECT count(*) AS count FROM projects", &[])
            .unwrap();
        let products = destination_conn
            .query_sync("SELECT count(*) AS count FROM products", &[])
            .unwrap();
        assert_eq!(projects[0].get_named::<i64>("count").unwrap(), 1);
        assert_eq!(
            products[0].get_named::<i64>("count").unwrap(),
            0,
            "a product committed after the first source read must not leak into the snapshot"
        );
        assert_eq!(
            interleaving_reader
                .inner
                .query_sync("SELECT count(*) AS count FROM products", &[])
                .unwrap()[0]
                .get_named::<i64>("count")
                .unwrap(),
            1,
            "the concurrent WAL commit must really have completed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_snapshot_preserves_same_process_writer_lock() {
        const CHILD_PATH_ENV: &str = "MCP_AGENT_MAIL_SHARE_LOCK_PROBE_PATH";
        const CHILD_TEST_NAME: &str =
            "snapshot::tests::live_snapshot_preserves_same_process_writer_lock";
        const CHILD_WITNESS: &str = "share-snapshot-child-observed-busy";

        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            // Competing writer through the runtime engine (FrankenSQLite): canonical
            // SQLite is never a mailbox writer and is not excluded by FrankenSQLite's
            // namespace/WAL-certificate coordination (br-0dw2c). The watchdog turns a
            // blocking engine into a visible failure instead of a hung suite.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(15));
                eprintln!("child lock probe watchdog: competing write did not fail within 15 s");
                std::process::exit(3);
            });
            let contender = DbConn::open_file(PathBuf::from(path).display().to_string())
                .expect("child opens competing runtime-engine connection");
            let _ = contender.execute_raw("PRAGMA busy_timeout = 10");
            let error = contender
                .execute_raw("BEGIN IMMEDIATE")
                .expect_err("separate process must not acquire the parent's reserved lock");
            let error = error.to_string().to_ascii_lowercase();
            assert!(
                error.contains("busy") || error.contains("locked"),
                "child must observe a lock refusal, not an unrelated error: {error}"
            );
            println!("{CHILD_WITNESS}");
            return;
        }

        fn assert_child_observes_busy(path: &Path) {
            let output = std::process::Command::new(
                std::env::current_exe().expect("resolve current test executable"),
            )
            .arg(CHILD_TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_PATH_ENV, path)
            .output()
            .expect("run competing child lock probe");
            assert!(
                output.status.success(),
                "child lock probe failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(CHILD_WITNESS),
                "child filter ran no causal lock probe: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("writer-lock.sqlite3");
        let destination = dir.path().join("writer-lock-copy.sqlite3");
        let seed = DbConn::open_file(source.display().to_string()).unwrap();
        seed.execute_raw("PRAGMA journal_mode = DELETE").unwrap();
        seed.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        seed.execute_raw("INSERT INTO projects VALUES (1, 'locked', '/locked', 0)")
            .unwrap();
        drop(seed);

        let writer = DbConn::open_file(source.display().to_string()).unwrap();
        writer.execute_raw("PRAGMA busy_timeout = 0").unwrap();
        writer.execute_raw("BEGIN IMMEDIATE").unwrap();
        let journal = PathBuf::from(format!("{}-journal", source.display()));
        assert!(
            !journal.exists(),
            "BEGIN IMMEDIATE without a write must not create a rollback journal"
        );

        assert_child_observes_busy(&source);
        create_sqlite_snapshot(&source, &destination, true)
            .expect("guarded live snapshot coexists with a reserved writer");
        assert_child_observes_busy(&source);

        writer.execute_raw("ROLLBACK").unwrap();
        let copy = CanonicalDbConn::open_file(destination.display().to_string()).unwrap();
        let rows = copy
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "locked");
    }

    #[test]
    fn snapshot_uses_absolute_candidate_for_missing_relative_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("shadow-source.sqlite3");
        let dest = dir.path().join("shadow-dest.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'shadow', '/shadow', 0)")
            .unwrap();
        drop(conn);

        let relative_source = PathBuf::from(source.strip_prefix("/").unwrap());
        assert!(!relative_source.exists());

        create_sqlite_snapshot(&relative_source, &dest, false).unwrap();
        assert!(dest.exists());
        assert!(!relative_source.exists());

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        let slug: String = rows[0].get_named("slug").unwrap();
        assert_eq!(slug, "shadow");
    }

    #[test]
    fn snapshot_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create source
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);
        std::fs::write(&dest, b"existing").unwrap();

        let result = create_sqlite_snapshot(&source, &dest, false);
        assert!(matches!(
            result,
            Err(ShareError::SnapshotDestinationExists { .. })
        ));
    }

    #[test]
    fn snapshot_publication_preserves_raced_destination_and_private_image() {
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let staged = stage_dir.path().join("private.sqlite3");
        let destination = dir.path().join("published.sqlite3");
        std::fs::write(&staged, b"complete private snapshot").unwrap();
        assert!(!destination.exists(), "name was available when selected");
        std::fs::write(&destination, b"another publisher won").unwrap();

        let error = publish_sqlite_snapshot(stage_dir, &staged, &destination, |_| {
            panic!("a collision must fail before the success durability barrier")
        })
        .expect_err("publication must not overwrite the raced-in destination");

        assert!(matches!(
            error,
            ShareError::SnapshotDestinationExists { .. }
        ));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"another publisher won"
        );
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"complete private snapshot"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_publication_preserves_raced_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let staged = stage_dir.path().join("private.sqlite3");
        let destination = dir.path().join("published.sqlite3");
        let absent_target = dir.path().join("absent.sqlite3");
        std::fs::write(&staged, b"complete private snapshot").unwrap();
        std::os::unix::fs::symlink(&absent_target, &destination).unwrap();
        assert!(!destination.exists(), "exists() misses a dangling symlink");

        let error = publish_sqlite_snapshot(stage_dir, &staged, &destination, |_| {
            panic!("symlink collision must not reach the durability barrier")
        })
        .expect_err("publication must not replace a dangling symlink");

        assert!(matches!(
            error,
            ShareError::SnapshotDestinationExists { .. }
        ));
        assert_eq!(std::fs::read_link(&destination).unwrap(), absent_target);
        assert!(!absent_target.exists());
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"complete private snapshot"
        );
    }

    #[test]
    fn snapshot_publication_reports_parent_sync_failure_without_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let staged = stage_dir.path().join("private.sqlite3");
        let destination = dir.path().join("published.sqlite3");
        std::fs::write(&staged, b"complete private snapshot").unwrap();

        let error = publish_sqlite_snapshot(stage_dir, &staged, &destination, |parent| {
            assert_eq!(parent, dir.path());
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"complete private snapshot"
            );
            Err(std::io::Error::other("injected directory sync failure"))
        })
        .expect_err("failed directory durability must not report success");

        assert!(
            error
                .to_string()
                .contains("published but parent durability")
        );
        assert!(
            error
                .to_string()
                .contains("injected directory sync failure")
        );
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"complete private snapshot"
        );
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            b"complete private snapshot"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_publication_syncs_parent_after_complete_image_is_visible() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let stage_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let staged = stage_dir.path().join("private.sqlite3");
        let destination = dir.path().join("published.sqlite3");
        std::fs::write(&staged, b"complete private snapshot").unwrap();
        let mut synced = false;

        publish_sqlite_snapshot(stage_dir, &staged, &destination, |parent| {
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"complete private snapshot"
            );
            sync_snapshot_parent(parent)?;
            synced = true;
            Ok(())
        })
        .unwrap();

        assert!(synced, "success must run the parent durability barrier");
        assert_eq!(std::fs::metadata(&destination).unwrap().nlink(), 1);
    }

    #[test]
    fn snapshot_refuses_destination_with_stale_sidecar_only() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");
        let dest_journal = PathBuf::from(format!("{}-journal", dest.display()));

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'sidecar', '/sidecar', 0)")
            .unwrap();
        drop(conn);

        std::fs::write(&dest_journal, b"stale-journal").unwrap();

        let err = create_sqlite_snapshot(&source, &dest, false)
            .expect_err("stale destination sidecars must block snapshot publish");
        assert!(matches!(err, ShareError::SnapshotDestinationExists { .. }));
        assert!(
            !dest.exists(),
            "snapshot must not publish a main database into a path with stale sidecars"
        );
        assert!(
            dest_journal.exists(),
            "blocking sidecar artifacts should be left untouched for explicit operator cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_destination_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);

        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();
        let dest = linked_parent.join("dest.sqlite3");

        let err = create_sqlite_snapshot(&source, &dest, false)
            .expect_err("symlinked destination parents must fail");
        assert!(matches!(err, ShareError::Io(_)));
        assert!(err.to_string().contains("symlinked snapshot directory"));
    }

    #[test]
    fn snapshot_preserves_runtime_recipient_and_reservation_state() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("runtime.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id INTEGER PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                recipients_json TEXT NOT NULL DEFAULT '{}', \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservation_releases (\
                reservation_id INTEGER PRIMARY KEY, \
                released_ts INTEGER NOT NULL\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE inbox_stats (\
                agent_id INTEGER PRIMARY KEY, \
                total_count INTEGER NOT NULL DEFAULT 0, \
                unread_count INTEGER NOT NULL DEFAULT 0, \
                ack_pending_count INTEGER NOT NULL DEFAULT 0, \
                last_message_ts INTEGER\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE tool_metrics_snapshots (\
                id INTEGER PRIMARY KEY, \
                collected_ts INTEGER NOT NULL, \
                tool_name TEXT NOT NULL, \
                calls INTEGER NOT NULL DEFAULT 0, \
                errors INTEGER NOT NULL DEFAULT 0, \
                cluster TEXT NOT NULL DEFAULT '', \
                capabilities_json TEXT NOT NULL DEFAULT '[]', \
                complexity TEXT NOT NULL DEFAULT 'unknown', \
                latency_avg_ms REAL, \
                latency_min_ms REAL, \
                latency_max_ms REAL, \
                latency_p50_ms REAL, \
                latency_p95_ms REAL, \
                latency_p99_ms REAL, \
                latency_is_slow INTEGER NOT NULL DEFAULT 0\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (\
                1, 7, 11, 'br-1', 'subject', 'body', 'high', 1, 12345, \
                '{\"to\":[\"Alice\"],\"cc\":[\"Bob\"],\"bcc\":[\"Carol\"]}', \
                '[{\"path\":\"a.txt\"}]'\
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO file_reservation_releases VALUES (42, 9001)")
            .unwrap();
        conn.execute_raw("INSERT INTO inbox_stats VALUES (11, 5, 2, 1, 12345)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO tool_metrics_snapshots VALUES (\
                3, 222, 'send_message', 9, 1, 'messaging', '[\"attachments\"]', \
                'medium', 12.5, 8.0, 20.0, 11.0, 18.0, 19.0, 1\
            )",
        )
        .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT recipients_json, attachments FROM messages WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        let attachments: String = rows[0].get_named("attachments").unwrap();
        assert_eq!(
            recipients_json,
            "{\"to\":[\"Alice\"],\"cc\":[\"Bob\"],\"bcc\":[\"Carol\"]}"
        );
        assert_eq!(attachments, "[{\"path\":\"a.txt\"}]");

        let rows = copy_conn
            .query_sync(
                "SELECT released_ts FROM file_reservation_releases WHERE reservation_id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let released_ts: String = rows[0].get_named("released_ts").unwrap();
        assert_eq!(released_ts, "9001");

        let rows = copy_conn
            .query_sync(
                "SELECT total_count FROM inbox_stats WHERE agent_id = 11",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let total_count: i64 = rows[0].get_named("total_count").unwrap();
        assert_eq!(total_count, 5);

        let rows = copy_conn
            .query_sync(
                "SELECT tool_name, capabilities_json FROM tool_metrics_snapshots WHERE id = 3",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let tool_name: String = rows[0].get_named("tool_name").unwrap();
        let capabilities_json: String = rows[0].get_named("capabilities_json").unwrap();
        assert_eq!(tool_name, "send_message");
        assert_eq!(capabilities_json, "[\"attachments\"]");
    }

    #[test]
    fn snapshot_preserves_agent_recovery_fields() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("runtime.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (\
                id INTEGER PRIMARY KEY, \
                project_id INTEGER NOT NULL, \
                name TEXT NOT NULL, \
                reaper_exempt INTEGER NOT NULL DEFAULT 0, \
                registration_token TEXT\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents \
             (id, project_id, name, reaper_exempt, registration_token) \
             VALUES (7, 3, 'RecoveryAgent', 1, 'registration-secret')",
        )
        .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT reaper_exempt, registration_token FROM agents WHERE id = 7",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("reaper_exempt").unwrap(), 1);
        assert_eq!(
            rows[0]
                .get_named::<Option<String>>("registration_token")
                .unwrap()
                .as_deref(),
            Some("registration-secret")
        );
    }

    #[test]
    fn snapshot_preserves_multiple_recipient_kinds_for_same_agent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("runtime.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (\
                message_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                kind TEXT NOT NULL, \
                read_ts INTEGER, \
                ack_ts INTEGER, \
                PRIMARY KEY(message_id, agent_id, kind)\
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 7, 'to', 111, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 7, 'cc', NULL, 222)")
            .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT kind, read_ts, ack_ts \
                 FROM message_recipients \
                 WHERE message_id = 1 AND agent_id = 7 \
                 ORDER BY kind",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "snapshot should preserve both recipient rows"
        );

        let cc_kind: String = rows[0].get_named("kind").unwrap();
        let cc_read_ts: Option<i64> = rows[0].get_named("read_ts").unwrap();
        let cc_ack_ts: Option<String> = rows[0].get_named("ack_ts").unwrap();
        assert_eq!(cc_kind, "cc");
        assert_eq!(cc_read_ts, None);
        assert_eq!(cc_ack_ts.as_deref(), Some("222"));

        let to_kind: String = rows[1].get_named("kind").unwrap();
        let to_read_ts: Option<String> = rows[1].get_named("read_ts").unwrap();
        let to_ack_ts: Option<String> = rows[1].get_named("ack_ts").unwrap();
        assert_eq!(to_kind, "to");
        assert_eq!(to_read_ts.as_deref(), Some("111"));
        assert_eq!(to_ack_ts, None);
    }

    #[test]
    fn snapshot_keeps_legacy_messages_and_defaults_missing_recipients_json() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id INTEGER PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 7, 11, 'br-legacy', 'subject', 'body', 'normal', 0, 12345, '[]')",
        )
        .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT subject, recipients_json FROM messages WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let subject: String = rows[0].get_named("subject").unwrap();
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        assert_eq!(subject, "subject");
        assert_eq!(recipients_json, "{}");
    }

    #[test]
    fn snapshot_errors_on_non_integer_pagination_key() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bad_key.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id TEXT PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                recipients_json TEXT NOT NULL DEFAULT '{}', \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES ('oops', 7, 11, 'br-bad', 'subject', 'body', 'normal', 0, 12345, '{}', '[]')",
        )
        .unwrap();
        drop(conn);

        let err = create_sqlite_snapshot(&source, &dest, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unexpected non-integer pagination column messages.id"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn snapshot_failure_does_not_leave_partial_destination_or_block_retry() {
        let dir = tempfile::tempdir().unwrap();
        let bad_source = dir.path().join("bad.sqlite3");
        let good_source = dir.path().join("good.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(bad_source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id TEXT PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                recipients_json TEXT NOT NULL DEFAULT '{}', \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES ('oops', 7, 11, 'br-bad', 'subject', 'body', 'normal', 0, 12345, '{}', '[]')",
        )
        .unwrap();
        drop(conn);

        let err = create_sqlite_snapshot(&bad_source, &dest, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("unexpected non-integer pagination column messages.id"),
            "unexpected error: {err}"
        );
        assert!(
            !dest.exists(),
            "failed snapshot attempts must not leave partial destination files behind"
        );

        let conn = DbConn::open_file(good_source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'retry', '/retry', 0)")
            .unwrap();
        drop(conn);

        create_sqlite_snapshot(&good_source, &dest, false)
            .expect("retry should succeed because the failed attempt left no destination behind");

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        let slug: String = rows[0].get_named("slug").unwrap();
        assert_eq!(slug, "retry");
    }

    /// Full pipeline integration: snapshot → scope/scrub/finalize →
    /// canonical bundle export → sign → verify
    #[test]
    fn full_pipeline_integration() {
        use crate::crypto::{sign_manifest, verify_bundle};
        let dir = tempfile::tempdir().unwrap();

        // 1. Create a seeded source database with FrankenSQLite (like runtime).
        let source = dir.path().join("source.sqlite3");
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
             program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
             inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', \
             attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, \
             agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, \
             reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', \
             released_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, \
             a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, \
             status TEXT DEFAULT 'pending', reason TEXT DEFAULT '', \
             created_ts TEXT DEFAULT '', updated_ts TEXT DEFAULT '', expires_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'myproj', '/test/proj', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Alice', 'claude-code', 'opus', 'testing', '', '', 'auto', 'auto')",
        ).unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'T1', 'Hello', 'Body text with api_key=SECRET123', \
             'normal', 0, '2026-01-01', '[{\"type\":\"file\",\"path\":\"test.txt\",\"media_type\":\"text/plain\"}]')",
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        drop(conn);

        // Create an attachment file
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("test.txt"), b"attachment content").unwrap();

        let snapshot = dir.path().join("snapshot.sqlite3");
        let context = create_snapshot_context(
            &source,
            &snapshot,
            &[],
            crate::ScrubPreset::Standard,
            SnapshotPurpose::Publish,
        )
        .unwrap();
        assert!(context.snapshot_path.exists());
        assert!(!context.scope.projects.is_empty());
        assert_eq!(
            context.scope.projects[0].human_key,
            "[project path redacted: myproj]"
        );
        assert!(context.scrub_summary.secrets_replaced >= 0);

        let output = dir.path().join("bundle");
        let export = crate::export_bundle_from_snapshot_context(
            &context,
            &output,
            &storage,
            &crate::BundleExportConfig {
                allow_absolute_attachment_paths: true,
                ..crate::BundleExportConfig::default()
            },
        )
        .unwrap();
        assert_eq!(export.attachment_manifest.stats.inline, 1); // small file → inline
        assert!(export.chunk_manifest.is_none());
        assert!(
            export
                .viewer_assets
                .iter()
                .any(|path| path == "viewer/index.html")
        );
        assert!(output.join("viewer/data/messages.json").exists());
        assert!(output.join("viewer/index.html").exists());
        assert!(output.join("viewer/pages/index.html").exists());
        assert!(export.static_render.pages_generated > 0);
        assert!(output.join("manifest.json").exists());
        assert!(output.join("README.md").exists());
        assert!(output.join("HOW_TO_DEPLOY.md").exists());
        assert!(output.join("index.html").exists());
        assert!(output.join("_headers").exists());
        assert!(output.join(".nojekyll").exists());

        // 13. Verify manifest.json is valid JSON with sorted keys
        let manifest_text = std::fs::read_to_string(output.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["schema_version"], "0.1.0");
        assert_eq!(manifest["database"]["path"], "mailbox.sqlite3");
        assert_eq!(manifest["database"]["sha256"], export.db_sha256);

        // Keys should be alphabetically sorted
        if let Some(obj) = manifest.as_object() {
            let keys: Vec<&String> = obj.keys().collect();
            let mut sorted_keys = keys.clone();
            sorted_keys.sort();
            assert_eq!(keys, sorted_keys, "top-level keys should be sorted");
        }

        // 14. Sign and verify
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, [42u8; 32]).unwrap();
        sign_manifest(
            &output.join("manifest.json"),
            &key_path,
            &output.join("manifest.sig.json"),
            false,
        )
        .unwrap();

        let verify = verify_bundle(&output, None).unwrap();
        assert!(verify.signature_checked);
        assert!(verify.signature_verified);

        // 15. The signed manifest covers every file the export wrote: a swapped
        // viewer page fails verification; the hosting config the deploy wizard
        // rewrites per provider does not.
        assert!(verify.files_checked && verify.files_verified, "{verify:?}");
        let listed = manifest["files"].as_object().expect("files map");
        for path in [
            "viewer/index.html",
            "viewer/data/messages.json",
            "mailbox.sqlite3",
        ] {
            assert!(listed.contains_key(path), "{path} must be hashed");
        }
        for path in [
            "manifest.json",
            "manifest.sig.json",
            "_headers",
            ".nojekyll",
        ] {
            assert!(!listed.contains_key(path), "{path} must not be hashed");
        }
        std::fs::write(output.join("_headers"), "/*\n  X-Provider: rewritten\n").unwrap();
        assert!(verify_bundle(&output, None).unwrap().files_verified);
        let viewer = output.join("viewer/index.html");
        std::fs::write(&viewer, b"<script>exfiltrate()</script>").unwrap();
        let tampered = verify_bundle(&output, None).unwrap();
        assert!(
            tampered.signature_verified,
            "the manifest itself is untouched"
        );
        assert!(!tampered.files_verified);
        assert!(
            tampered
                .error
                .as_deref()
                .is_some_and(|error| error.contains("viewer/index.html")),
            "{tampered:?}"
        );
    }

    #[test]
    fn create_snapshot_context_normalizes_integer_timestamps_for_share_queries() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("integer-ts.sqlite3");
        let snapshot = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER DEFAULT 0)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
             program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
             inception_ts INTEGER DEFAULT 0, last_active_ts INTEGER DEFAULT 0, \
             attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts INTEGER DEFAULT 0, attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO projects VALUES (1, 'myproj', '/test/proj', 1707000000000000)",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Alice', 'claude-code', 'opus', 'testing', \
             1707000000000000, 1707000001000000, 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'T1', 'Hello', 'Body', 'normal', 0, \
             1707000002000000, '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO message_recipients VALUES (1, 1, 'to', 1707000003000000, NULL)",
        )
        .unwrap();
        drop(conn);

        let context = create_snapshot_context(
            &source,
            &snapshot,
            &[],
            crate::ScrubPreset::Standard,
            SnapshotPurpose::Publish,
        )
        .unwrap();
        assert!(context.snapshot_path.exists());

        let copy_conn = SqliteConnection::open_file(snapshot.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT created_at FROM projects WHERE id = 1", &[])
            .unwrap();
        let created_at: String = rows[0].get_named("created_at").unwrap();
        assert_eq!(created_at, "1707000000000000");

        let rows = copy_conn
            .query_sync(
                "SELECT inception_ts, last_active_ts FROM agents WHERE id = 1",
                &[],
            )
            .unwrap();
        let inception_ts: String = rows[0].get_named("inception_ts").unwrap();
        let last_active_ts: String = rows[0].get_named("last_active_ts").unwrap();
        assert_eq!(inception_ts, "1707000000000000");
        assert_eq!(last_active_ts, "1707000001000000");

        let rows = copy_conn
            .query_sync("SELECT created_ts FROM messages WHERE id = 1", &[])
            .unwrap();
        let created_ts: String = rows[0].get_named("created_ts").unwrap();
        assert_eq!(created_ts, "1707000002000000");

        let rows = copy_conn
            .query_sync(
                "SELECT read_ts FROM message_recipients WHERE message_id = 1 AND agent_id = 1",
                &[],
            )
            .unwrap();
        let read_ts: Option<String> = rows[0].get_named("read_ts").unwrap();
        assert_eq!(read_ts, None);

        let rows = copy_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM file_reservations", &[])
            .unwrap();
        let file_reservation_count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(file_reservation_count, 0);

        let rows = copy_conn
            .query_sync("SELECT COUNT(*) AS cnt FROM agent_links", &[])
            .unwrap();
        let agent_link_count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(agent_link_count, 0);
    }

    #[test]
    fn export_bundle_replaces_stale_owned_outputs_but_preserves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();

        let source = dir.path().join("source.sqlite3");
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
             program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
             inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', \
             attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, \
             agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, \
             reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', \
             released_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, \
             a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, \
             status TEXT DEFAULT 'pending', reason TEXT DEFAULT '', \
             created_ts TEXT DEFAULT '', updated_ts TEXT DEFAULT '', expires_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'myproj', '/test/proj', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Alice', 'claude-code', 'opus', 'testing', '', '', 'auto', 'auto')",
        ).unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'T1', 'Hello', 'Body text', \
             'normal', 0, '2026-01-01', '[{\"type\":\"file\",\"path\":\"test.txt\",\"media_type\":\"text/plain\"}]')",
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        drop(conn);

        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("test.txt"), b"attachment content").unwrap();

        let snapshot = dir.path().join("snapshot.sqlite3");
        let context = create_snapshot_context(
            &source,
            &snapshot,
            &[],
            crate::ScrubPreset::Archive,
            SnapshotPurpose::Publish,
        )
        .unwrap();
        assert_eq!(context.scope.projects[0].human_key, "/test/proj");

        let output = dir.path().join("bundle");
        std::fs::create_dir_all(output.join("viewer/data")).unwrap();
        std::fs::create_dir_all(output.join("viewer/pages/messages")).unwrap();
        std::fs::create_dir_all(output.join("attachments")).unwrap();
        std::fs::create_dir_all(output.join("chunks")).unwrap();
        std::fs::write(output.join("viewer/obsolete.js"), "stale viewer asset").unwrap();
        std::fs::write(
            output.join("viewer/data/redaction_audit.json"),
            "{\"stale\":true}",
        )
        .unwrap();
        std::fs::write(
            output.join("viewer/pages/messages/999.html"),
            "<html>stale page</html>",
        )
        .unwrap();
        std::fs::write(output.join("attachments/stale.bin"), "stale attachment").unwrap();
        std::fs::write(output.join("chunks/00000.bin"), "stale chunk").unwrap();
        std::fs::write(output.join("chunks.sha256"), "stale checksum").unwrap();
        std::fs::write(
            output.join("mailbox.sqlite3.config.json"),
            "{\"stale\":true}",
        )
        .unwrap();
        std::fs::write(output.join("manifest.sig.json"), "{\"stale\":true}").unwrap();
        std::fs::write(output.join("mailbox.sqlite3-journal"), "stale journal").unwrap();
        std::fs::write(output.join("mailbox.sqlite3-wal"), "stale wal").unwrap();
        std::fs::write(output.join("mailbox.sqlite3-shm"), "stale shm").unwrap();
        std::fs::write(output.join("keep.txt"), "preserve me").unwrap();

        let export = crate::export_bundle_from_snapshot_context(
            &context,
            &output,
            &storage,
            &crate::BundleExportConfig {
                allow_absolute_attachment_paths: true,
                inline_attachment_threshold: 1,
                detach_attachment_threshold: 1024,
                chunk_threshold: usize::MAX,
                ..crate::BundleExportConfig::default()
            },
        )
        .unwrap();

        assert!(export.chunk_manifest.is_none());
        assert!(output.join("viewer/index.html").exists());
        assert!(output.join("viewer/data/messages.json").exists());
        assert!(output.join("viewer/pages/messages/1.html").exists());
        assert!(output.join("attachments").exists());
        assert!(
            !output.join("viewer/obsolete.js").exists(),
            "stale viewer assets must be removed"
        );
        assert!(
            !output.join("viewer/data/redaction_audit.json").exists(),
            "stale optional data files must be removed when not regenerated"
        );
        assert!(
            !output.join("viewer/pages/messages/999.html").exists(),
            "stale rendered pages must be removed"
        );
        assert!(
            !output.join("attachments/stale.bin").exists(),
            "stale attachment files must be removed"
        );
        assert!(
            !output.join("chunks").exists(),
            "stale chunk directories must be removed when chunking is disabled"
        );
        assert!(!output.join("chunks.sha256").exists());
        assert!(!output.join("mailbox.sqlite3.config.json").exists());
        assert!(!output.join("manifest.sig.json").exists());
        assert!(!output.join("mailbox.sqlite3-journal").exists());
        assert!(!output.join("mailbox.sqlite3-wal").exists());
        assert!(!output.join("mailbox.sqlite3-shm").exists());
        assert_eq!(
            std::fs::read_to_string(output.join("keep.txt")).unwrap(),
            "preserve me"
        );
    }
}
