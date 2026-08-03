//! Read-only, snapshot-consistent aggregate reads for the demo-pack exporter.
//!
//! The exporter must never obtain write capability on the private source
//! mailbox, must not create or mutate any file next to it, and the aggregate
//! counts it publishes must all describe one database state even while a
//! live mailbox is being written concurrently. All three properties are
//! enforced here rather than trusted:
//!
//! - SQLite never opens the source path. The database and any complete WAL
//!   pair are copied into a private temporary directory between two strong
//!   source fingerprints (file identity, metadata, and SHA-256). The copied
//!   bytes must exactly match the stable second fingerprint; partial WAL pairs,
//!   rollback journals, path replacement, and concurrent source drift all fail
//!   closed before SQL runs;
//! - the verified private copy is opened with `SQLITE_OPEN_READONLY`, and that
//!   contract is proven at open time: a `PRAGMA user_version = <current>` header
//!   write — a semantic no-op that still travels the real write path — must be
//!   rejected with the exact `SQLITE_READONLY` code before any read runs;
//! - every aggregate query executes inside a single deferred read
//!   transaction on the verified copy, so all published counts describe one
//!   stable source snapshot without any source-side lock or shared-memory write.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use same_file::Handle;
use sha2::{Digest, Sha256};
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection, sqlite_error_code};

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

/// Identity and strong content fingerprint for one source file.
#[derive(Debug, PartialEq, Eq)]
struct FileFingerprint {
    identity: Handle,
    len: u64,
    modified: SystemTime,
    sha256: [u8; 32],
}

impl FileFingerprint {
    fn capture(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        let identity = Handle::from_file(file.try_clone()?)?;
        let metadata = file.metadata()?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
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

/// Stable source state around the private snapshot copy. SHM contents are not
/// logical database data and may change due to other readers; its file identity
/// is still bound so a replaced/incomplete WAL pair cannot cross the copy.
#[derive(Debug, PartialEq, Eq)]
struct SourceFingerprint {
    database: FileFingerprint,
    wal: Option<FileFingerprint>,
    shm: Option<Handle>,
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
        if wal_exists != shm_exists {
            return Err(
                "source database has an incomplete WAL sidecar pair; retry after the writer stabilizes"
                    .into(),
            );
        }
        Ok(Self {
            database: FileFingerprint::capture(path)?,
            wal: wal_exists
                .then(|| FileFingerprint::capture(&wal_path))
                .transpose()?,
            shm: shm_exists
                .then(|| Handle::from_path(&shm_path))
                .transpose()?,
        })
    }
}

struct VerifiedSourceSnapshot {
    directory: tempfile::TempDir,
    database_path: PathBuf,
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
    let temporary_root = std::env::temp_dir();
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
    let directory = tempfile::Builder::new()
        .prefix("agent-mail-dashboard-export-")
        .tempdir()?;
    let database_path = directory.path().join("mailbox.sqlite3");
    std::fs::copy(source_path, &database_path)?;

    let (source_wal, _) = wal_sidecar_paths(source_path);
    let (snapshot_wal, _) = wal_sidecar_paths(&database_path);
    if before.wal.is_some() {
        std::fs::copy(&source_wal, &snapshot_wal)?;
    }

    let after = SourceFingerprint::capture(source_path)?;
    if after != before {
        return Err(
            "source database or WAL identity/content changed while creating the private snapshot; \
             retry the export"
                .into(),
        );
    }

    let copied_database = FileFingerprint::capture(&database_path)?;
    if !copied_database.content_matches(&after.database) {
        return Err("private database copy does not match the verified source bytes".into());
    }
    match &after.wal {
        Some(source_wal_fingerprint) => {
            let copied_wal = FileFingerprint::capture(&snapshot_wal)?;
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
}

/// Open the source mailbox database strictly read-only, failing closed.
///
/// SQLite never opens `path`. A stable byte-for-byte snapshot is copied into a
/// private temporary directory first, and every SQLite operation targets only
/// that verified copy. This avoids source-side WAL shared-memory writes while
/// retaining committed WAL frames in the aggregate snapshot.
///
/// In both modes the read-only contract is proven, not trusted: a
/// `PRAGMA user_version = <current>` header write — a semantic no-op that
/// still travels SQLite's real write path — must be rejected with
/// `SQLITE_READONLY`, otherwise the export aborts. (`BEGIN IMMEDIATE` is
/// deliberately not the probe — modern SQLite permits it on read-only
/// connections and only fails the first actual write.)
pub fn open_source_read_only(path: &str) -> Result<SourceConnection, Box<dyn std::error::Error>> {
    let source_path = std::fs::canonicalize(path)?;
    let snapshot = copy_verified_source(&source_path)?;
    let snapshot_path = snapshot
        .database_path
        .to_str()
        .ok_or("private snapshot path is not valid UTF-8")?
        .to_owned();
    let mut config = SqliteConfig::file(snapshot_path);
    config.flags = OpenFlags::read_only();
    let connection = SqliteConnection::open(&config)?;

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
            if code.primary() != sqlmodel_sqlite::ffi::SQLITE_READONLY {
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
    })
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

impl SourceConnection {
    /// The underlying read-only connection (write attempts are rejected by
    /// SQLite itself; the open probe proved it).
    #[must_use]
    pub fn connection(&self) -> &SqliteConnection {
        &self.connection
    }
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
    let connection = &source.connection;
    let now_micros = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;

    connection.execute_raw("BEGIN")?;
    let result =
        read_aggregates_in_transaction(connection, now_micros, source.expected_user_version);
    if result.is_ok() {
        connection.execute_raw("COMMIT")?;
    } else {
        // Best-effort: the connection is read-only, so there is nothing to
        // undo; this just releases the read snapshot.
        let _ = connection.execute_raw("ROLLBACK");
    }

    result
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
        file_reservations: count(
            connection,
            &format!(
                "SELECT COUNT(*) AS c FROM file_reservations \
                 WHERE released_ts IS NULL AND expires_ts > {now_micros}"
            ),
        )?,
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
        let result = open_source_read_only(&path.to_string_lossy());
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
    fn read_only_open_rejects_partial_wal_pairs_without_mutating_files() {
        for sidecar_suffix in ["-wal", "-shm"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("mailbox.sqlite3");
            let path_string = path.to_string_lossy().into_owned();
            let writer = SqliteConnection::open_file(path_string.clone()).expect("open database");
            create_schema(&writer);
            insert_coherent_round(&writer);
            drop(writer);

            let sidecar_path = PathBuf::from(format!("{path_string}{sidecar_suffix}"));
            std::fs::write(&sidecar_path, b"incomplete synthetic sidecar")
                .expect("create partial sidecar fixture");
            let before = snapshot_directory(dir.path());

            let result = open_source_read_only(&path_string);
            let error = match result {
                Ok(_) => panic!("partial WAL pair unexpectedly opened"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("incomplete WAL sidecar pair"),
                "partial pair failed for an unrelated reason: {error}"
            );
            assert_eq!(
                snapshot_directory(dir.path()),
                before,
                "rejected partial WAL pair must remain byte-for-byte untouched"
            );
        }
    }

    #[test]
    fn read_only_open_rejects_rollback_journals_without_mutating_files() {
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
        let error = match open_source_read_only(&path_string) {
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

    #[test]
    fn read_only_open_rejects_writes_and_write_locks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let writer = writer_db(&path.to_string_lossy());
        create_schema(&writer);
        insert_coherent_round(&writer);
        drop(writer);

        let reader = open_source_read_only(&path.to_string_lossy()).expect("read-only open");
        let write_attempt = reader
            .connection()
            .execute_raw("INSERT INTO projects DEFAULT VALUES");
        assert!(
            write_attempt.is_err(),
            "read-only connection accepted a write"
        );
        let counts = read_aggregates_snapshot(&reader).expect("aggregates");
        assert_eq!(counts.projects, 1);
        assert_eq!(counts.ack_pending, 1);
    }

    #[test]
    fn aggregate_snapshot_is_immune_to_source_schema_change_after_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();
        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let reader = open_source_read_only(&path_string).expect("read-only open");
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
        let reader = open_source_read_only(&path_string).expect("read-only open");

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
    fn source_commit_during_private_copy_fails_closed() {
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
    fn aggregates_are_mutually_consistent_under_concurrent_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let reader = open_source_read_only(&path_string).expect("read-only open");
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
    fn live_wal_export_reads_committed_frames_without_mutating_source_files() {
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
        let reader = open_source_read_only(&path_string).expect("read-only snapshot open");
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
    fn export_reads_do_not_create_or_mutate_source_files() {
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
        let reader = open_source_read_only(&path_string).expect("read-only open");
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
