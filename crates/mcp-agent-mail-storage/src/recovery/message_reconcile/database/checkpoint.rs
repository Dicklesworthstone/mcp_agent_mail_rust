//! Restart-safe scheduling hints for the live archive reconciler.
//!
//! Scan positions and retry debt are published together. A crash before that
//! publication replays the previous bounded pass instead of forgetting a
//! deferred ID behind a newer high-water mark. Nothing in this file is archive
//! authority: every restored ID still traverses the normal live-source checks.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use asupersync::Cx;
use fastmcp_core::block_on;
use fs2::FileExt;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, corruption_circuit_breaker};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    DeferredRetry, MAX_RETRY_BACKOFF, MAX_RETRY_IDS, ReconcileCursor, ReconcileReport, outcome,
    reconcile_message_batch_inner, source_error, validate_pool_binding,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_CHECKPOINT_BYTES: u64 = 256 * 1024;

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    database: PathBuf,
    archive: PathBuf,
    // The file identity, not its length/mtime: ordinary commits must not
    // invalidate a checkpoint. Replacement databases must start a fresh scan.
    generation: String,
}

impl Binding {
    fn capture(pool: &DbPool, config: &Config) -> io::Result<Self> {
        let database = std::fs::canonicalize(pool.sqlite_path())?;
        Self::at(&database, &config.storage_root)
    }

    fn at(database: &Path, archive: &Path) -> io::Result<Self> {
        let database = std::fs::canonicalize(database)?;
        let archive = std::fs::canonicalize(archive)?;
        let metadata = std::fs::metadata(&database)?;
        if !metadata.is_file() {
            return Err(invalid("checkpoint source is not a regular database file"));
        }
        let created = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| (time.as_secs(), time.subsec_nanos()));
        #[cfg(unix)]
        let generation = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}:{created:?}", metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let generation = format!(
            "{:?}",
            created.ok_or_else(|| invalid("checkpoint source has no stable file identity"))?
        );
        Ok(Self {
            database,
            archive,
            generation,
        })
    }

    fn path(&self) -> io::Result<PathBuf> {
        let parent = self
            .database
            .parent()
            .ok_or_else(|| invalid("checkpoint source has no parent directory"))?;
        let mut digest = Sha256::new();
        digest.update(self.database.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(self.archive.as_os_str().as_encoded_bytes());
        // Keep maintenance hints beside SQLite, not inside an archive Git tree.
        // Different archive roots for the same database cannot share progress.
        Ok(parent.join(format!(
            ".am-message-reconcile-{}.json",
            hex::encode(digest.finalize())
        )))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    id: i64,
    attempts: u32,
    wait_us: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Conflict {
    id: i64,
    wait_us: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    binding: Binding,
    saved_at_us: i64,
    tail_after: Option<i64>,
    backfill_ceiling: Option<i64>,
    next_lane_is_history: bool,
    next_lane_is_retry: bool,
    retries: Vec<Retry>,
    conflicts: Vec<Conflict>,
}

fn wait_us(until: Instant, now: Instant) -> u64 {
    u64::try_from(
        until
            .saturating_duration_since(now)
            .min(MAX_RETRY_BACKOFF)
            .as_micros(),
    )
    .unwrap_or(u64::MAX)
}

impl Snapshot {
    fn capture(binding: Binding, cursor: &ReconcileCursor, now: Instant, wall_us: i64) -> Self {
        Self {
            version: SCHEMA_VERSION,
            binding,
            saved_at_us: wall_us,
            tail_after: cursor.tail_after,
            backfill_ceiling: cursor.backfill_ceiling,
            next_lane_is_history: cursor.next_lane_is_history,
            next_lane_is_retry: cursor.next_lane_is_retry,
            retries: cursor
                .retries
                .iter()
                .map(|retry| Retry {
                    id: retry.id,
                    attempts: retry.attempts,
                    wait_us: wait_us(retry.ready_at, now),
                })
                .collect(),
            conflicts: cursor
                .conflict_backoff
                .iter()
                .filter(|(_, until)| *until > now)
                .map(|(id, until)| Conflict {
                    id: *id,
                    wait_us: wait_us(*until, now),
                })
                .collect(),
        }
    }

    fn validate(&self, binding: &Binding) -> io::Result<()> {
        if self.version != SCHEMA_VERSION || &self.binding != binding {
            return Err(invalid(
                "checkpoint version or source/archive generation does not match",
            ));
        }
        if self.saved_at_us <= 0
            || self.tail_after.is_some_and(|id| id < 0)
            || self.backfill_ceiling.is_some_and(|id| id < 0)
            || self.retries.len() > MAX_RETRY_IDS
            || self.conflicts.len() > MAX_RETRY_IDS
        {
            return Err(invalid("checkpoint scheduling bounds are invalid"));
        }
        let mut ids = HashSet::new();
        let max_wait = u64::try_from(MAX_RETRY_BACKOFF.as_micros()).unwrap_or(u64::MAX);
        for (id, wait) in self
            .retries
            .iter()
            .map(|retry| (retry.id, retry.wait_us))
            .chain(
                self.conflicts
                    .iter()
                    .map(|conflict| (conflict.id, conflict.wait_us)),
            )
        {
            if id <= 0 || wait > max_wait || !ids.insert(id) {
                return Err(invalid(
                    "checkpoint contains invalid or duplicate deferred IDs",
                ));
            }
        }
        Ok(())
    }

    fn restore(self, cursor: &mut ReconcileCursor, now: Instant, wall_us: i64) {
        // Instant is process-local. Persist only a bounded delay and discount
        // elapsed wall time. A backwards clock cannot defer work indefinitely.
        let elapsed = u64::try_from(wall_us.saturating_sub(self.saved_at_us)).unwrap_or(0);
        let ready_at = |wait: u64| now + Duration::from_micros(wait.saturating_sub(elapsed));
        cursor.tail_after = self.tail_after;
        cursor.backfill_ceiling = self.backfill_ceiling;
        cursor.next_lane_is_history = self.next_lane_is_history;
        cursor.next_lane_is_retry = self.next_lane_is_retry;
        cursor.retries = self
            .retries
            .into_iter()
            .map(|retry| DeferredRetry {
                id: retry.id,
                attempts: retry.attempts,
                ready_at: ready_at(retry.wait_us),
            })
            .collect();
        cursor.conflict_backoff = self
            .conflicts
            .into_iter()
            .filter_map(|conflict| {
                let until = ready_at(conflict.wait_us);
                (until > now).then_some((conflict.id, until))
            })
            .collect();
        // Never restore a previous process's write-behind grace anchor, pool
        // identity, or persistence state from untrusted scheduling metadata.
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_sidecar(path: &Path) -> io::Result<()> {
    if crate::path_existing_prefix_has_symlink(path)
        .map_err(|error| invalid(&error.to_string()))?
    {
        return Err(invalid("checkpoint path has a symlinked component"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(invalid("checkpoint path is not a regular file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(invalid("checkpoint path has multiple hard links"));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

struct Store {
    binding: Binding,
    path: PathBuf,
    // A nonblocking, per-source lock serializes checkpoint read/run/publish.
    // Keep the descriptor until the pass finishes; never wait on another server.
    _lock: File,
}

impl Store {
    fn open(binding: Binding) -> io::Result<Self> {
        let path = binding.path()?;
        check_sidecar(&path)?;
        let lock_path = path.with_extension("json.lock");
        check_sidecar(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
        }
        let lock = options.open(&lock_path)?;
        if !lock.metadata()?.is_file() {
            return Err(invalid("checkpoint lock is not a regular file"));
        }
        check_sidecar(&lock_path)?;
        FileExt::try_lock_exclusive(&lock)?;
        Ok(Self {
            binding,
            path,
            _lock: lock,
        })
    }

    fn load(&self) -> io::Result<Option<Snapshot>> {
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &self.path,
            MAX_CHECKPOINT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: Snapshot = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(&format!("checkpoint cannot be decoded: {error}")))?;
        snapshot.validate(&self.binding)?;
        Ok(Some(snapshot))
    }

    fn save(&self, cursor: &ReconcileCursor) -> io::Result<()> {
        // Promotion is not held through filesystem I/O. Refuse to attach old
        // progress to a replacement database that appeared during the pass.
        let current = Binding::at(&self.binding.database, &self.binding.archive)?;
        if current != self.binding {
            return Err(invalid("checkpoint source changed during reconciliation"));
        }
        check_sidecar(&self.path)?;
        let snapshot = Snapshot::capture(
            current,
            cursor,
            Instant::now(),
            mcp_agent_mail_db::now_micros(),
        );
        snapshot.validate(&self.binding)?;
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|error| invalid(&format!("checkpoint cannot be encoded: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CHECKPOINT_BYTES {
            return Err(invalid("checkpoint byte budget exceeded"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("checkpoint has no parent directory"))?;
        let mut candidate = tempfile::Builder::new()
            .prefix(".am-message-reconcile-")
            .tempfile_in(parent)?;
        candidate.write_all(&bytes)?;
        candidate.as_file().sync_all()?;
        candidate.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn warn_unavailable(error: &io::Error) {
    tracing::warn!(
        target: "maintenance", event = "message_archive_checkpoint_unavailable",
        reason = %error,
        "archive reconciliation continues in memory; restart progress is not durable"
    );
}

pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
) -> Result<ReconcileReport, String> {
    if stop.load(Ordering::Acquire)
        || cx.checkpoint().is_err()
        || corruption_circuit_breaker().is_tripped()
    {
        return reconcile_message_batch_inner(cx, pool, config, cursor, stop);
    }
    let scope;
    let store = {
        let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
            .ok_or("message reconciliation deferred: recovery promotion or admission contention")?;
        validate_pool_binding(pool, config)?;
        // A query-only pool must not create or update even a hint sidecar.
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let rows = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(source_error)?;
        if rows.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize message archive repair".into());
        }
        drop(conn);
        scope = pool.sqlite_identity_key();
        if cursor.source_identity != scope {
            *cursor = ReconcileCursor {
                source_identity: scope.clone(),
                settled_before_us: cursor.settled_before_us,
                persist_progress: cursor.persist_progress,
                ..Default::default()
            };
        }
        match Binding::capture(pool, config).and_then(Store::open) {
            Ok(store) => Some(store),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(
                    "message reconciliation deferred: checkpoint is owned by another worker".into(),
                );
            }
            Err(error) => {
                warn_unavailable(&error);
                None
            }
        }
    };
    let mut restored = false;
    if !cursor.checkpoint_dirty
        && let Some(store) = &store
    {
        match store.load() {
            Ok(Some(snapshot)) => {
                snapshot.restore(cursor, Instant::now(), mcp_agent_mail_db::now_micros());
                restored = true;
            }
            Ok(None) => {}
            Err(error) => warn_unavailable(&error),
        }
    }
    let mut result = reconcile_message_batch_inner(cx, pool, config, cursor, stop);
    cursor.checkpoint_dirty = true;
    let saved = if let Some(store) = &store {
        if pool.sqlite_identity_key() == scope && cursor.source_identity == scope {
            match store.save(cursor) {
                Ok(()) => {
                    cursor.checkpoint_dirty = false;
                    true
                }
                Err(error) => {
                    warn_unavailable(&error);
                    false
                }
            }
        } else {
            warn_unavailable(&invalid(
                "checkpoint pool generation changed during reconciliation",
            ));
            false
        }
    } else {
        false
    };
    if let Ok(report) = &mut result {
        report.checkpoint_restored = restored;
        report.checkpoint_saved = Some(saved);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("mail.sqlite3");
        let archive = temp.path().join("archive");
        std::fs::write(&database, b"source generation").unwrap();
        std::fs::create_dir(&archive).unwrap();
        (temp, database, archive)
    }

    fn debt(now: Instant) -> ReconcileCursor {
        ReconcileCursor {
            tail_after: Some(90_000),
            backfill_ceiling: Some(200),
            next_lane_is_history: true,
            next_lane_is_retry: true,
            retries: VecDeque::from([
                DeferredRetry {
                    id: 7,
                    attempts: 0,
                    ready_at: now,
                },
                DeferredRetry {
                    id: 80_000,
                    attempts: 6,
                    ready_at: now + Duration::from_secs(30),
                },
            ]),
            conflict_backoff: VecDeque::from([(19, now + Duration::from_secs(60))]),
            ..ReconcileCursor::settled_before(123)
        }
    }

    #[test]
    fn checkpoint_restores_low_and_high_retry_debt_and_lane_turns_together() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        let binding = Binding::at(&database, &archive).unwrap();
        let snapshot = Snapshot::capture(binding, &debt(now), now, 1_000_000);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let restored: Snapshot = serde_json::from_slice(&bytes).unwrap();
        restored
            .validate(&Binding::at(&database, &archive).unwrap())
            .unwrap();
        let mut cursor = ReconcileCursor::settled_before(999);
        cursor.source_identity = "new process pool".into();
        restored.restore(&mut cursor, now, 11_000_000);
        assert_eq!(cursor.tail_after, Some(90_000));
        assert_eq!(cursor.backfill_ceiling, Some(200));
        assert!(cursor.next_lane_is_history);
        assert!(cursor.next_lane_is_retry);
        assert_eq!(
            cursor.retries.iter().map(|retry| retry.id).collect::<Vec<_>>(),
            [7, 80_000]
        );
        assert_eq!(cursor.retries[0].ready_at, now);
        assert_eq!(cursor.retries[1].attempts, 6);
        assert_eq!(cursor.retries[1].ready_at, now + Duration::from_secs(20));
        assert_eq!(cursor.conflict_backoff[0].1, now + Duration::from_secs(50));
        assert_eq!(cursor.settled_before_us, 999);
        assert_eq!(cursor.source_identity, "new process pool");
        assert!(cursor.persist_progress);
    }

    #[test]
    fn checkpoint_clock_changes_cannot_pin_retry_work() {
        let (_temp, database, archive) = fixture();
        let now = Instant::now();
        for wall_us in [1, 2_000_000, i64::MAX] {
            let snapshot = Snapshot::capture(
                Binding::at(&database, &archive).unwrap(),
                &debt(now),
                now,
                2_000_000,
            );
            let mut cursor = ReconcileCursor::default();
            snapshot.restore(&mut cursor, now, wall_us);
            assert!(cursor.retries.iter().all(|retry| {
                retry.ready_at >= now && retry.ready_at <= now + MAX_RETRY_BACKOFF
            }));
            if wall_us == i64::MAX {
                assert!(cursor.retries.iter().all(|retry| retry.ready_at == now));
                assert!(cursor.conflict_backoff.is_empty());
            }
        }
    }

    #[test]
    fn checkpoint_bounds_and_duplicate_ids_fail_closed() {
        let (_temp, database, archive) = fixture();
        let binding = Binding::at(&database, &archive).unwrap();
        let now = Instant::now();
        for change in ["version", "negative", "duplicate", "cross_lane", "delay", "count"] {
            let mut snapshot = Snapshot::capture(
                Binding::at(&database, &archive).unwrap(),
                &debt(now),
                now,
                1_000_000,
            );
            match change {
                "version" => snapshot.version += 1,
                "negative" => snapshot.backfill_ceiling = Some(-1),
                "duplicate" => snapshot.retries[1].id = snapshot.retries[0].id,
                "cross_lane" => snapshot.conflicts[0].id = snapshot.retries[0].id,
                "delay" => snapshot.retries[0].wait_us = u64::MAX,
                _ => {
                    snapshot.retries = (1..=MAX_RETRY_IDS + 1)
                        .map(|id| Retry {
                            id: i64::try_from(id).unwrap(),
                            attempts: 0,
                            wait_us: 0,
                        })
                        .collect();
                }
            }
            assert!(snapshot.validate(&binding).is_err(), "{change}");
        }
    }

    #[test]
    fn checkpoint_is_source_generation_and_archive_root_scoped() {
        let (_temp, database, archive) = fixture();
        let before = Binding::at(&database, &archive).unwrap();
        std::fs::write(&database, b"an ordinary database write changes its length").unwrap();
        assert_eq!(before, Binding::at(&database, &archive).unwrap());
        let other = archive.with_file_name("other-archive");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(
            before.path().unwrap(),
            Binding::at(&database, &other).unwrap().path().unwrap()
        );
        std::fs::rename(&database, database.with_extension("held")).unwrap();
        std::fs::write(&database, b"replacement generation").unwrap();
        assert_ne!(before, Binding::at(&database, &archive).unwrap());
    }

    #[test]
    fn checkpoint_publish_survives_reopening_and_rejects_stale_source() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        assert!(store.load().unwrap().is_none());
        store.save(&debt(Instant::now())).unwrap();
        let bytes = std::fs::read(&store.path).unwrap();
        drop(store);
        let reopened = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        let snapshot = reopened.load().unwrap().unwrap();
        assert_eq!(snapshot.tail_after, Some(90_000));
        assert_eq!(snapshot.retries.len(), 2);
        std::fs::rename(&database, database.with_extension("held")).unwrap();
        std::fs::write(&database, b"replacement").unwrap();
        assert!(reopened.save(&debt(Instant::now())).is_err());
        assert_eq!(std::fs::read(&reopened.path).unwrap(), bytes);
    }

    #[test]
    fn checkpoint_lock_contention_is_nonblocking_and_preserves_progress() {
        let (_temp, database, archive) = fixture();
        let first = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        first.save(&debt(Instant::now())).unwrap();
        let bytes = std::fs::read(&first.path).unwrap();
        let error = Store::open(Binding::at(&database, &archive).unwrap())
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
        drop(first);
        assert!(Store::open(Binding::at(&database, &archive).unwrap()).is_ok());
    }

    #[test]
    fn checkpoint_reads_reject_malformed_and_oversized_records() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        for bytes in [
            b"{partial".to_vec(),
            vec![b' '; MAX_CHECKPOINT_BYTES as usize + 1],
        ] {
            std::fs::write(&store.path, bytes).unwrap();
            assert!(store.load().is_err());
        }
    }

    #[test]
    fn unpublished_progress_replays_the_last_atomic_checkpoint() {
        let (_temp, database, archive) = fixture();
        let store = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        let mut cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        // Simulate a crash between advancing the in-memory scan and publishing
        // its checkpoint. The old high-water mark and its debt remain paired.
        cursor.tail_after = Some(100_000);
        cursor.retries.clear();
        drop(store);
        let restarted = Store::open(Binding::at(&database, &archive).unwrap()).unwrap();
        let saved = restarted.load().unwrap().unwrap();
        assert_eq!(saved.tail_after, Some(90_000));
        assert_eq!(
            saved.retries.iter().map(|retry| retry.id).collect::<Vec<_>>(),
            [7, 80_000]
        );
    }

    #[test]
    fn restarted_worker_repairs_retry_debt_before_history_wrap_without_changing_mailbox() {
        super::super::tests::retention_fixture(40, |cx, pool, config| {
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            cursor.source_identity = pool.sqlite_identity_key();
            cursor.tail_after = Some(930);
            cursor.backfill_ceiling = Some(920);
            cursor.next_lane_is_retry = true;
            cursor.defer_transient(901, Instant::now());
            let store = Store::open(Binding::capture(pool, config).unwrap()).unwrap();
            store.save(&cursor).unwrap();
            drop(store);

            let mut restarted = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let report = super::super::reconcile_message_batch(
                cx,
                pool,
                config,
                &mut restarted,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(report.checkpoint_restored);
            assert_eq!(report.checkpoint_saved, Some(true));
            assert_eq!(report.repaired, super::super::MAX_REPAIRS_PER_BATCH);
            assert!(restarted.retries.is_empty());
            let prepared = super::super::prepare_message(cx, pool, 901).unwrap();
            let archive = crate::open_archive(config, "project").unwrap().unwrap();
            let paths = crate::message_paths_for_bundle(
                &archive,
                &prepared.message,
                &prepared.sender,
                &prepared.recipients,
            )
            .unwrap()
            .0;
            assert!(
                paths.canonical.is_file(),
                "low retry ID must not wait for history wrap"
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 40);
            let rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = 901",
                    &[],
                )
                .unwrap();
            assert_eq!(rows.len(), 2);
            for row in rows {
                assert_eq!(row.get_named::<i64>("read_ts").unwrap(), 2_000_000);
                assert_eq!(row.get_named::<i64>("ack_ts").unwrap(), 2_500_000);
            }
        });
    }

    #[test]
    fn stopped_worker_does_not_create_a_progress_sidecar() {
        super::super::tests::retention_fixture(1, |cx, pool, config| {
            let path = Binding::capture(pool, config).unwrap().path().unwrap();
            let mut cursor = ReconcileCursor::settled_before(mcp_agent_mail_db::now_micros());
            let report = super::super::reconcile_message_batch(
                cx,
                pool,
                config,
                &mut cursor,
                &AtomicBool::new(true),
            )
            .unwrap();
            assert!(report.interrupted);
            assert_eq!(report.scanned, 0);
            assert_eq!(report.checkpoint_saved, None);
            assert!(!path.exists());
            assert!(!path.with_extension("json.lock").exists());
        });
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_never_follows_symlinked_or_hardlinked_sidecars() {
        let (_temp, database, archive) = fixture();
        let binding = Binding::at(&database, &archive).unwrap();
        let path = binding.path().unwrap();
        let evidence = archive.join("preserved-evidence");
        std::fs::write(&evidence, b"do not replace").unwrap();
        std::os::unix::fs::symlink(&evidence, &path).unwrap();
        assert!(Store::open(binding).is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"do not replace");
        std::fs::rename(&path, path.with_extension("held-symlink")).unwrap();
        std::fs::hard_link(&evidence, &path).unwrap();
        assert!(Store::open(Binding::at(&database, &archive).unwrap()).is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"do not replace");
    }
}
