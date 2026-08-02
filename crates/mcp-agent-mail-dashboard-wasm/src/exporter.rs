//! Read-only, snapshot-consistent aggregate reads for the demo-pack exporter.
//!
//! The exporter must never obtain write capability on the private source
//! mailbox, must not create or mutate any file next to it, and the aggregate
//! counts it publishes must all describe one database state even while a
//! live mailbox is being written concurrently. All three properties are
//! enforced here rather than trusted:
//!
//! - the source is opened with `SQLITE_OPEN_READONLY` and the contract is
//!   proven fail-closed at open time: a `PRAGMA user_version = <current>`
//!   header write — a semantic no-op that still travels the real write path
//!   — must be rejected with `SQLITE_READONLY` before any read runs;
//! - a plain read-only open of a WAL database still CREATES `-shm`/`-wal`
//!   sidecars when they are absent (observed on SQLite 3.46.1), so the open
//!   is mode-split: when the sidecars already exist (a live mailbox) the
//!   reader merely joins the existing WAL and creates nothing; when the
//!   source is quiescent (no sidecars) it is opened `immutable=1`, which
//!   takes no locks and creates no files, and quiescence is re-verified
//!   after the read — if a writer appeared mid-read the export fails closed
//!   rather than publish counts read outside SQLite's locking protocol;
//! - every aggregate query executes inside a single deferred read
//!   transaction, so SQLite serves them all from one stable snapshot (WAL
//!   snapshot isolation, or the shared lock in rollback-journal mode).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};

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

/// Stat fingerprint used to re-verify that a quiescent source stayed
/// quiescent across an `immutable=1` read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuiescentStamp {
    len: u64,
    modified: SystemTime,
    wal_exists: bool,
    shm_exists: bool,
}

fn wal_sidecar_paths(path: &Path) -> (PathBuf, PathBuf) {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    (PathBuf::from(wal), PathBuf::from(shm))
}

fn quiescent_stamp(path: &Path) -> Result<QuiescentStamp, Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    let (wal, shm) = wal_sidecar_paths(path);
    Ok(QuiescentStamp {
        len: metadata.len(),
        modified: metadata.modified()?,
        wal_exists: wal.exists(),
        shm_exists: shm.exists(),
    })
}

/// Percent-encode the characters that would change URI interpretation.
fn uri_escape_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace('#', "%23")
        .replace('?', "%3F")
}

/// A strictly read-only handle on the source mailbox database.
pub struct SourceConnection {
    connection: SqliteConnection,
    /// `Some` when the source was quiescent at open time and was therefore
    /// opened `immutable=1`; the stamp re-verifies quiescence after reads.
    immutable_stamp: Option<QuiescentStamp>,
    path: PathBuf,
}

/// Open the source mailbox database strictly read-only, failing closed.
///
/// Mode selection: when the WAL sidecars (`-wal`/`-shm`) already exist the
/// source is a live mailbox and a plain read-only open joins the existing
/// WAL without creating anything. When the source is quiescent, a plain
/// read-only open would CREATE those sidecars (observed on SQLite 3.46.1),
/// so it is opened `immutable=1` instead — no locks, no file creation — and
/// [`read_aggregates_snapshot`] re-verifies quiescence after the read.
///
/// In both modes the read-only contract is proven, not trusted: a
/// `PRAGMA user_version = <current>` header write — a semantic no-op that
/// still travels SQLite's real write path — must be rejected with
/// `SQLITE_READONLY`, otherwise the export aborts. (`BEGIN IMMEDIATE` is
/// deliberately not the probe — modern SQLite permits it on read-only
/// connections and only fails the first actual write.)
pub fn open_source_read_only(path: &str) -> Result<SourceConnection, Box<dyn std::error::Error>> {
    let source_path = PathBuf::from(path);
    let (wal, shm) = wal_sidecar_paths(&source_path);
    let live_wal = wal.exists() || shm.exists();

    let (connection, immutable_stamp) = if live_wal {
        let mut config = SqliteConfig::file(path);
        config.flags = OpenFlags::read_only();
        (SqliteConnection::open(&config)?, None)
    } else {
        let stamp = quiescent_stamp(&source_path)?;
        let uri = format!("file:{}?mode=ro&immutable=1", uri_escape_path(path));
        let mut config = SqliteConfig::file(uri);
        config.flags = OpenFlags::read_only();
        config.flags.uri = true;
        (SqliteConnection::open(&config)?, Some(stamp))
    };

    let user_version = connection
        .query_sync("PRAGMA user_version", &[])?
        .first()
        .ok_or("PRAGMA user_version returned no row")?
        .get_named::<i64>("user_version")?;
    if connection
        .execute_raw(&format!("PRAGMA user_version = {user_version}"))
        .is_ok()
    {
        return Err(
            "read-only contract not established: source connection accepted a header write; \
             refusing to export"
                .into(),
        );
    }

    Ok(SourceConnection {
        connection,
        immutable_stamp,
        path: source_path,
    })
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
/// For a source that was quiescent at open time (`immutable=1` mode, which
/// reads outside SQLite's locking protocol), quiescence is re-verified after
/// the read: if the database or its WAL sidecars changed while the export
/// ran, the counts cannot be trusted and the export fails closed.
pub fn read_aggregates_snapshot(
    source: &SourceConnection,
) -> Result<AggregateCounts, Box<dyn std::error::Error>> {
    let connection = &source.connection;
    let now_micros = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;

    connection.execute_raw("BEGIN")?;
    let result = read_aggregates_in_transaction(connection, now_micros);
    if result.is_ok() {
        connection.execute_raw("COMMIT")?;
    } else {
        // Best-effort: the connection is read-only, so there is nothing to
        // undo; this just releases the read snapshot.
        let _ = connection.execute_raw("ROLLBACK");
    }

    if let Some(stamp) = &source.immutable_stamp {
        let now = quiescent_stamp(&source.path)?;
        if now != *stamp {
            return Err(
                "source database changed during an immutable read; the exported counts \
                 cannot be trusted — rerun the export against the live database"
                    .into(),
            );
        }
    }

    result
}

fn read_aggregates_in_transaction(
    connection: &SqliteConnection,
    now_micros: i64,
) -> Result<AggregateCounts, Box<dyn std::error::Error>> {
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
            "INSERT INTO message_recipients (message_id, ack_ts) VALUES (1, NULL)".to_string(),
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
    fn immutable_read_fails_closed_when_writer_appears_mid_read() {
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

        // Quiescent at open time -> immutable=1 mode.
        let reader = open_source_read_only(&path_string).expect("read-only open");

        // A writer shows up while the exporter still holds the immutable
        // handle. Immutable reads take no locks, so SQLite cannot protect
        // this case — the exporter's own quiescence re-verification must.
        let late_writer = writer_db(&path_string);
        insert_coherent_round(&late_writer);
        drop(late_writer);

        let result = read_aggregates_snapshot(&reader);
        assert!(
            result.is_err(),
            "immutable-mode export must fail closed when the source changes mid-read, \
             got {result:?}"
        );
    }

    #[test]
    fn aggregates_are_mutually_consistent_under_concurrent_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mailbox.sqlite3");
        let path_string = path.to_string_lossy().into_owned();

        let writer = writer_db(&path_string);
        create_schema(&writer);
        insert_coherent_round(&writer);

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer_path = path_string.clone();
        let writer_thread = std::thread::spawn(move || {
            let connection = writer_db(&writer_path);
            let mut rounds: u64 = 1; // schema setup already inserted round one
            while !writer_stop.load(Ordering::Acquire) {
                insert_coherent_round(&connection);
                rounds += 1;
            }
            rounds
        });

        let reader = open_source_read_only(&path_string).expect("read-only open");
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
            observed.iter().all(|&count| count >= 1 && count <= rounds),
            "observed counts must fall within the committed-round range"
        );
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

        let snapshot_dir = |label: &str| -> std::collections::BTreeMap<String, Vec<u8>> {
            std::fs::read_dir(dir.path())
                .expect(label)
                .map(|entry| {
                    let entry = entry.expect(label);
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let bytes = std::fs::read(entry.path()).expect(label);
                    (name, bytes)
                })
                .collect()
        };

        let before = snapshot_dir("snapshot before");
        let reader = open_source_read_only(&path_string).expect("read-only open");
        let counts = read_aggregates_snapshot(&reader).expect("aggregates");
        drop(reader);
        let after = snapshot_dir("snapshot after");

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
