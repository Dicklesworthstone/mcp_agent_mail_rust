//! Step 1: SQLite snapshot creation via SQL-level dump and restore.
//!
//! Creates an atomic, clean canonical SQLite copy of the source database suitable for
//! offline manipulation (scoping, scrubbing, finalization with FTS5/VACUUM).
//!
//! Instead of a byte-level file copy we read schema + data through the runtime
//! driver and re-create them in a fresh destination file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
use sqlmodel_core::Value;

use crate::ShareError;

const SQLITE_SNAPSHOT_SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

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
        ],
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
            "subject",
            "body_md",
            "importance",
            "ack_required",
            "created_ts",
            "recipients_json",
            "attachments",
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

/// Create a snapshot of a live FrankenSQLite database at `destination`.
///
/// The source must already be admitted by FrankenSQLite and retain its complete
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
/// FrankenSQLite namespace authority fails closed.
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
    // disposable export image that will be renamed into place. Creating that
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
            transfer_tables_in_consistent_read(&src, &dst_conn)?;
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
            transfer_tables_in_consistent_read(&src, &dst_conn)?;
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
    // Staging ran with synchronous=OFF (br-gi4z3), so force the finished image
    // to disk once, here, before the rename publishes it.
    std::fs::File::open(&staged_dest)
        .and_then(|file| file.sync_all())
        .map_err(ShareError::Io)?;
    std::fs::rename(&staged_dest, &dest).map_err(ShareError::Io)?;

    Ok(dest)
}

/// Relax durability on the staged destination while tables stream in.
///
/// br-gi4z3: the destination lives in a `.snapshot-stage.` tempdir that is
/// discarded on any failure and only published via checkpoint + `sync_all` +
/// rename on success, so per-statement durability during staging buys nothing.
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
fn transfer_tables_in_consistent_read<S: SnapshotSource>(
    src: &S,
    dst: &CanonicalDbConn,
) -> Result<(), ShareError> {
    src.execute_snapshot("BEGIN")
        .map_err(|e| ShareError::Sqlite {
            message: format!("cannot begin consistent source snapshot read: {e}"),
        })?;

    match transfer_tables(src, dst) {
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
fn transfer_tables<S: SnapshotSource>(src: &S, dst: &CanonicalDbConn) -> Result<(), ShareError> {
    for table in KNOWN_TABLES {
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
        let select_columns = quoted_column_list(&available_columns);
        let page_by_column = table
            .page_by_column
            .filter(|column| source_columns.contains(*column));
        let mut last_page_value: i64 = -1;
        loop {
            let (select_sql, params): (String, Vec<Value>) =
                if let Some(page_by_column) = page_by_column {
                    (
                        format!(
                            "SELECT {select_columns} FROM \"{}\" WHERE \"{page_by_column}\" > ?1 \
                         ORDER BY \"{page_by_column}\" ASC LIMIT 1000",
                            table.name
                        ),
                        vec![Value::BigInt(last_page_value)],
                    )
                } else {
                    (
                        format!("SELECT {select_columns} FROM \"{}\"", table.name),
                        vec![],
                    )
                };

            let rows =
                src.query_snapshot(&select_sql, &params)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("SELECT from {} failed: {e}", table.name),
                    })?;

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
                if let Some(page_by_column) = page_by_column {
                    last_page_value = extract_page_value(row, table.name, page_by_column)?;
                }
                dst.execute_sync(&insert_sql, &values)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("INSERT into {} failed: {e}", table.name),
                    })?;
            }
            dst.execute_sync("COMMIT", &[])
                .map_err(|e| ShareError::Sqlite {
                    message: format!("COMMIT for {} page failed: {e}", table.name),
                })?;
            if page_by_column.is_none() {
                break;
            }
        }
    }
    Ok(())
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

/// Build INSERT OR REPLACE SQL for a destination table.
fn build_insert(table: &str, columns: &[&str]) -> String {
    let col_list = quoted_column_list(columns);
    let placeholders = (0..columns.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT OR REPLACE INTO \"{table}\" ({col_list}) VALUES ({placeholders})")
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
    Ok(extract_column_names(&rows))
}

fn extract_column_names(rows: &[sqlmodel_core::Row]) -> HashSet<String> {
    rows.iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect()
}

fn extract_page_value(
    row: &sqlmodel_core::Row,
    table: &str,
    page_by_column: &str,
) -> Result<i64, ShareError> {
    let Some(val) = row.get_by_name(page_by_column) else {
        return Err(ShareError::Sqlite {
            message: format!("missing pagination column {page_by_column} while copying {table}"),
        });
    };
    match val {
        Value::BigInt(v) => Ok(*v),
        Value::Int(v) => Ok(i64::from(*v)),
        _ => Err(ShareError::Sqlite {
            message: format!("unexpected non-integer pagination column {table}.{page_by_column}"),
        }),
    }
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

/// Full snapshot preparation pipeline for a live FrankenSQLite mailbox.
///
/// 1. Create snapshot
/// 2. Apply project scope
/// 3. Scrub data
/// 4. Finalize (FTS, materialized views, performance indexes, VACUUM)
///
/// The source assumptions are the same as [`create_sqlite_snapshot`]: a
/// complete, valid FrankenSQLite namespace pair is mandatory and the source
/// is opened only through the guarded read-only seam.
pub fn create_snapshot_context(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
) -> Result<SnapshotContext, ShareError> {
    create_sqlite_snapshot(source, snapshot_path, true)?;
    finish_snapshot_context(snapshot_path, project_filters, scrub_preset)
}

/// Full snapshot preparation pipeline for a private canonical SQLite source.
///
/// The source must be a caller-owned, engine-exclusive archive reconstruction
/// or materialized snapshot. Never pass a live FrankenSQLite mailbox path;
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
) -> Result<SnapshotContext, ShareError> {
    create_private_canonical_sqlite_snapshot(source, snapshot_path, true)?;
    finish_snapshot_context(snapshot_path, project_filters, scrub_preset)
}

fn finish_snapshot_context(
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
) -> Result<SnapshotContext, ShareError> {
    let mut scope = crate::apply_project_scope(snapshot_path, project_filters)?;
    let scrub_summary = crate::scrub_snapshot(snapshot_path, scrub_preset)?;
    if !matches!(scrub_preset, crate::ScrubPreset::Archive) {
        crate::scrub::redact_scope_project_human_keys(&mut scope);
    }
    let finalize = crate::finalize_export_db(snapshot_path)?;

    Ok(SnapshotContext {
        snapshot_path: snapshot_path.to_path_buf(),
        scope,
        scrub_summary,
        fts_enabled: finalize.fts_enabled,
    })
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
        let writer_path = source.clone();
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
            let contender = CanonicalDbConn::open_file(PathBuf::from(path).display().to_string())
                .expect("child opens competing canonical connection");
            contender
                .execute_raw("PRAGMA busy_timeout = 0")
                .expect("child disables busy retry");
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
        let context =
            create_snapshot_context(&source, &snapshot, &[], crate::ScrubPreset::Standard).unwrap();
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

        let context =
            create_snapshot_context(&source, &snapshot, &[], crate::ScrubPreset::Standard).unwrap();
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
        let context =
            create_snapshot_context(&source, &snapshot, &[], crate::ScrubPreset::Archive).unwrap();
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
