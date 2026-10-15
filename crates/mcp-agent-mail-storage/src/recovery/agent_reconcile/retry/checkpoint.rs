//! Restart-safe scheduling for agent lifecycle archive recovery.
//!
//! The checkpoint contains IDs, roster position and bounded retry delays, never
//! a profile or authorization token. The cursor owns one nonblocking sidecar
//! lock for its lifetime, so independent workers cannot overwrite retry debt.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, UNIX_EPOCH};

use asupersync::Cx;
use fastmcp_core::block_on;
use fs2::FileExt;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, corruption_circuit_breaker};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::{
    AgentReconcileCursor, AgentReconcileReport, checkpoint, outcome, reconcile_agent_batch,
    source_error, validate_source,
};
use super::{Entry, MAX_BACKOFF, MAX_RETRY_IDS};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Default)]
pub(super) struct Persistence {
    binding: Option<Binding>,
    restore_eligible: bool,
    restore_attempted: bool,
    warned: bool,
    store: Option<Store>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    database: PathBuf,
    archive: PathBuf,
    file_identity: String,
    generation: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Binding {
    fn at(database: &Path, archive: &Path, generation: String) -> io::Result<Self> {
        if generation.is_empty()
            || generation.len() > 128
            || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("checkpoint database generation is invalid"));
        }
        let database = std::fs::canonicalize(database)?;
        let archive = std::fs::canonicalize(archive)?;
        let metadata = std::fs::metadata(&database)?;
        if !metadata.is_file() || !archive.is_dir() {
            return Err(invalid("checkpoint source or archive has an invalid file type"));
        }
        let created = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| (time.as_secs(), time.subsec_nanos()));
        #[cfg(unix)]
        let file_identity = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}:{created:?}", metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let file_identity = format!(
            "{:?}",
            created.ok_or_else(|| invalid("source has no stable file identity"))?
        );
        Ok(Self {
            database,
            archive,
            file_identity,
            generation,
        })
    }

    // The caller holds promotion admission. Query-only and source checks run
    // before creating even a hint/lock file. Logical generation also detects
    // an in-place recovery that retained the database's physical file identity.
    fn capture(cx: &Cx, pool: &DbPool, config: &Config) -> Result<Self, String> {
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let mode = conn.query_sync("PRAGMA query_only", &[]).map_err(source_error)?;
        if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize agent archive repair".into());
        }
        let rows = conn
            .query_sync("SELECT generation_id FROM db_identity WHERE singleton = 0", &[])
            .map_err(source_error)?;
        let [row] = rows.as_slice() else {
            return Err("agent checkpoint requires one database generation".into());
        };
        let generation = row.get_named::<String>("generation_id").map_err(source_error)?;
        drop(conn);
        Self::at(Path::new(pool.sqlite_path()), &config.storage_root, generation)
            .map_err(|error| error.to_string())
    }

    fn path(&self) -> io::Result<PathBuf> {
        let parent = self.database.parent().ok_or_else(|| invalid("source has no parent"))?;
        let mut digest = Sha256::new();
        digest.update(self.database.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(self.archive.as_os_str().as_encoded_bytes());
        Ok(parent.join(format!(
            ".am-agent-reconcile-{}.json",
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
struct Snapshot {
    version: u32,
    binding: Binding,
    saved_at_us: i64,
    after: i64,
    ceiling: Option<i64>,
    next_lane_is_retry: bool,
    retries: Vec<Retry>,
}

impl Snapshot {
    fn capture(binding: Binding, cursor: &AgentReconcileCursor, now: Instant, wall: i64) -> Self {
        Self {
            version: VERSION,
            binding,
            saved_at_us: wall,
            after: cursor.after,
            ceiling: cursor.ceiling,
            next_lane_is_retry: cursor.next_lane_is_retry,
            retries: cursor.retries.entries.iter().map(|entry| Retry {
                id: entry.id,
                attempts: entry.attempts,
                wait_us: u64::try_from(
                    entry.ready_at.saturating_duration_since(now).min(MAX_BACKOFF).as_micros()
                ).unwrap_or(u64::MAX),
            }).collect(),
        }
    }

    fn validate(&self, binding: &Binding) -> io::Result<()> {
        if self.version != VERSION || &self.binding != binding {
            return Err(invalid("checkpoint version or source/archive generation differs"));
        }
        if self.saved_at_us <= 0
            || self.after < 0
            || self.ceiling.is_some_and(|ceiling| ceiling < self.after)
            || self.retries.len() > MAX_RETRY_IDS
        {
            return Err(invalid("checkpoint scheduling bounds are invalid"));
        }
        let max_wait = u64::try_from(MAX_BACKOFF.as_micros()).unwrap_or(u64::MAX);
        let mut ids = HashSet::new();
        for retry in &self.retries {
            if retry.id <= 0 || retry.wait_us > max_wait || !ids.insert(retry.id) {
                return Err(invalid("checkpoint retry IDs or delays are invalid"));
            }
        }
        Ok(())
    }

    fn restore(self, cursor: &mut AgentReconcileCursor, now: Instant, wall: i64) {
        let elapsed = u64::try_from(wall.saturating_sub(self.saved_at_us)).unwrap_or(0);
        cursor.after = self.after;
        cursor.ceiling = self.ceiling;
        cursor.next_lane_is_retry = self.next_lane_is_retry;
        cursor.retries.entries = self.retries.into_iter().map(|retry| Entry {
            id: retry.id,
            attempts: retry.attempts,
            ready_at: now + Duration::from_micros(retry.wait_us.saturating_sub(elapsed)),
        }).collect();
        // Never import pool authority or ownership from scheduling metadata.
        // A backwards clock can delay a retry by at most MAX_BACKOFF.
    }
}

fn check_path(path: &Path) -> io::Result<()> {
    if crate::path_existing_prefix_has_symlink(path).map_err(|error| invalid(error.to_string()))? {
        return Err(invalid("checkpoint path contains a symlink"));
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

#[derive(Debug)]
struct Store {
    binding: Binding,
    path: PathBuf,
    lock_path: PathBuf,
    lock: File,
}

impl Store {
    fn open(binding: Binding) -> io::Result<Self> {
        let path = binding.path()?;
        check_path(&path)?;
        let lock_path = path.with_extension("json.lock");
        check_path(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
        }
        let lock = options.open(&lock_path)?;
        FileExt::try_lock_exclusive(&lock)?;
        let store = Self { binding, path, lock_path, lock };
        store.check_lock()?;
        Ok(store)
    }

    fn check_lock(&self) -> io::Result<()> {
        check_path(&self.lock_path)?;
        let opened = self.lock.metadata()?;
        let named = std::fs::symlink_metadata(&self.lock_path)?;
        if !opened.is_file() || !named.is_file() {
            return Err(invalid("checkpoint lock is not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.nlink() != 1 || (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
                return Err(invalid("checkpoint lock identity changed"));
            }
        }
        #[cfg(not(unix))]
        if opened.created()? != named.created()? {
            return Err(invalid("checkpoint lock identity changed"));
        }
        Ok(())
    }

    fn load(&self) -> io::Result<Option<Snapshot>> {
        self.check_lock()?;
        check_path(&self.path)?;
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(&self.path, MAX_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
        snapshot.validate(&self.binding)?;
        Ok(Some(snapshot))
    }

    fn save(&self, cursor: &AgentReconcileCursor) -> io::Result<()> {
        self.check_lock()?;
        check_path(&self.path)?;
        // Ordinary data writes keep this binding; physical replacement does not.
        if Binding::at(&self.binding.database, &self.binding.archive, self.binding.generation.clone())?
            != self.binding
        {
            return Err(invalid("checkpoint source file changed during reconciliation"));
        }
        let snapshot = Snapshot::capture(self.binding.clone(), cursor, Instant::now(), mcp_agent_mail_db::now_micros());
        snapshot.validate(&self.binding)?;
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| invalid(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(invalid("checkpoint exceeds its byte limit"));
        }
        let parent = self.path.parent().ok_or_else(|| invalid("checkpoint has no parent"))?;
        let mut temporary = tempfile::Builder::new().prefix(".am-agent-reconcile-").tempfile_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        self.check_lock()?;
        check_path(&self.path)?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn warn_unavailable(state: &mut Persistence, reason: &str) {
    if !state.warned {
        state.warned = true;
        tracing::warn!(target: "maintenance", event = "agent_archive_checkpoint_unavailable",
            reason, "agent profile repair continues in memory; restart progress is not durable");
    }
}

impl AgentReconcileCursor {
    /// Run the ordinary lifecycle reconciler with restart-safe scheduling hints.
    ///
    /// One cursor owns the sidecar lock until dropped or rebound. The checkpoint
    /// pairs its roster watermark with all retry IDs, bound to the configured
    /// database/root and physical/logical generation. It never contains profiles
    /// or tokens and does not relax the normal source or publication checks.
    /// Unavailable hints leave repair memory-only; a failed save cannot cause a
    /// later load to replace newer in-memory work with an older snapshot.
    ///
    /// # Errors
    /// Source/admission failures retain the ordinary reconciler's semantics.
    /// Another checkpoint owner refuses this pass without consuming any IDs.
    /// Other checkpoint I/O errors are logged without disabling archive repair.
    pub fn reconcile_checkpointed(
        &mut self,
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        stop: &AtomicBool,
    ) -> Result<AgentReconcileReport, String> {
        if checkpoint(cx, stop).is_err() || corruption_circuit_breaker().is_tripped() {
            return reconcile_agent_batch(cx, pool, config, self, stop);
        }
        let (scope, captured) = {
            let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
                .ok_or("agent reconciliation deferred: recovery promotion or admission contention")?;
            validate_source(pool, config)?;
            // Capture must succeed before sidecar mutation. In particular an
            // unreadable/query-only source is not merely a missing hint store.
            (pool.sqlite_identity_key(), Binding::capture(cx, pool, config)?)
        };
        let rebind = self.source_identity != scope
            || self.retries.persistence.binding.as_ref().is_some_and(|known| known != &captured);
        if rebind {
            *self = Self { source_identity: scope.clone(), ..Self::default() };
            self.retries.persistence.restore_eligible = true;
        }
        let mut state = std::mem::take(&mut self.retries.persistence);
        state.binding = Some(captured.clone());
        if let Some(store) = &state.store
            && let Err(error) = store.check_lock()
        {
            // A replaced lock no longer proves exclusive checkpoint ownership.
            // Do not run or publish from stale in-memory state under that lock.
            // A new cursor must acquire the named lock and restore fresh hints.
            *self = Self::default();
            return Err(format!("agent checkpoint ownership changed; rescan required: {error}"));
        }
        if state.store.is_none() {
            match Store::open(captured.clone()) {
                Ok(store) => state.store = Some(store),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.retries.persistence = state;
                    return Err("agent reconciliation deferred: checkpoint is owned by another worker".into());
                }
                Err(error) => warn_unavailable(&mut state, &error.to_string()),
            }
        }
        if !state.restore_attempted && state.restore_eligible
            && let Some(store) = &state.store
        {
            match store.load() {
                Ok(Some(snapshot)) => {
                    snapshot.restore(self, Instant::now(), mcp_agent_mail_db::now_micros());
                }
                Ok(None) => {}
                Err(error) => warn_unavailable(&mut state, &error.to_string()),
            }
        }
        // Mark this before executing a batch, including an interrupted/error
        // batch: its newer in-memory obligations must never be rolled back.
        state.restore_attempted = true;
        let result = reconcile_agent_batch(cx, pool, config, self, stop);
        if self.source_identity == scope
            && checkpoint(cx, stop).is_ok()
            && !corruption_circuit_breaker().is_tripped()
            && let Some(store) = &state.store
        {
            let saved = (|| -> Result<(), String> {
                {
                    let _admission = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
                        .ok_or("agent checkpoint save deferred by promotion")?;
                    validate_source(pool, config)?;
                    if Binding::capture(cx, pool, config)? != captured {
                        return Err("agent checkpoint source generation changed during reconciliation".into());
                    }
                }
                // Hint fsync never holds database-promotion admission. A later
                // replacement rejects this snapshot's OLD binding on restore.
                store.save(self).map_err(|error| error.to_string())
            })();
            match saved {
                Ok(()) => state.warned = false,
                Err(error) => warn_unavailable(&mut state, &error),
            }
        }
        if self.source_identity == scope {
            self.retries.persistence = state;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::sync::atomic::Ordering;

    fn binding(temp: &tempfile::TempDir) -> Binding {
        let database = temp.path().join("mail.sqlite3");
        let archive = temp.path().join("archive");
        std::fs::write(&database, b"source identity").unwrap();
        std::fs::create_dir(&archive).unwrap();
        Binding::at(&database, &archive, "aabb".into()).unwrap()
    }

    fn debt(now: Instant) -> AgentReconcileCursor {
        let mut cursor = AgentReconcileCursor {
            source_identity: "live-pool-only".into(),
            after: 432,
            ceiling: Some(480),
            next_lane_is_retry: true,
            ..Default::default()
        };
        cursor.retries.defer(401, now);
        cursor.retries.defer(401, now);
        cursor.retries.defer(402, now);
        cursor
    }

    #[test]
    fn snapshot_pairs_roster_position_fifo_debt_and_remaining_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        let bytes = serde_json::to_vec(&Snapshot::capture(binding.clone(), &debt(now), now, 1_000_000)).unwrap();
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        snapshot.validate(&binding).unwrap();
        let mut resumed = AgentReconcileCursor::default();
        snapshot.restore(&mut resumed, now, 1_500_000);
        assert_eq!((resumed.after, resumed.ceiling, resumed.next_lane_is_retry), (432, Some(480), true));
        assert!(resumed.source_identity.is_empty(), "a hint cannot restore pool authority");
        assert!(resumed.retries.persistence.store.is_none());
        assert_eq!(resumed.retries.ready_ids(now), [402]);
        assert_eq!(resumed.retries.ready_ids(now + Duration::from_millis(500)), [401, 402]);
        assert_eq!(resumed.retries.entries[0].attempts, 1);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("registration_token") && !text.contains("task_description"));
    }

    #[test]
    fn clock_changes_never_extend_restored_retry_delay_beyond_the_bound() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        for wall in [1, 1_500_000, 70_000_000, i64::MAX] {
            let mut original = debt(now);
            for _ in 0..100 {
                original.retries.defer(401, now);
            }
            let snapshot = Snapshot::capture(binding.clone(), &original, now, 2_000_000);
            let mut resumed = AgentReconcileCursor::default();
            snapshot.restore(&mut resumed, now, wall);
            assert!(resumed.retries.entries.iter().all(|entry| {
                entry.ready_at >= now && entry.ready_at <= now + MAX_BACKOFF
            }));
            if wall >= 70_000_000 {
                assert_eq!(resumed.retries.ready_ids(now), [402, 401]);
            }
        }
    }

    #[test]
    fn malformed_schedule_and_unknown_fields_cannot_restore_a_partial_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        for mutation in ["version", "time", "negative", "ceiling", "id", "duplicate", "wait", "count"] {
            let mut snapshot = Snapshot::capture(binding.clone(), &debt(now), now, 1_000_000);
            match mutation {
                "version" => snapshot.version += 1,
                "time" => snapshot.saved_at_us = 0,
                "negative" => snapshot.after = -1,
                "ceiling" => snapshot.ceiling = Some(snapshot.after - 1),
                "id" => snapshot.retries[0].id = 0,
                "duplicate" => snapshot.retries[1].id = snapshot.retries[0].id,
                "wait" => snapshot.retries[0].wait_us = u64::MAX,
                _ => snapshot.retries = (1..=MAX_RETRY_IDS + 1).map(|id| Retry {
                    id: i64::try_from(id).unwrap(), attempts: 0, wait_us: 0,
                }).collect(),
            }
            assert!(snapshot.validate(&binding).is_err(), "{mutation}");
        }
        let mut value = serde_json::to_value(Snapshot::capture(binding, &debt(now), now, 1_000_000)).unwrap();
        value["profile"] = serde_json::json!({"retired_at": null});
        assert!(serde_json::from_value::<Snapshot>(value).is_err());
    }

    #[test]
    fn binding_rejects_replacement_and_logical_generation_but_not_ordinary_writes() {
        let temp = tempfile::tempdir().unwrap();
        let before = binding(&temp);
        std::fs::write(&before.database, b"ordinary data write changes length and mtime").unwrap();
        assert_eq!(Binding::at(&before.database, &before.archive, "aabb".into()).unwrap(), before);
        let logical = Binding::at(&before.database, &before.archive, "ccdd".into()).unwrap();
        let snapshot = Snapshot::capture(before.clone(), &debt(Instant::now()), Instant::now(), 1);
        assert!(snapshot.validate(&logical).is_err());
        let other = temp.path().join("other-archive");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(before.path().unwrap(), Binding::at(&before.database, &other, "aabb".into()).unwrap().path().unwrap());
        std::fs::rename(&before.database, temp.path().join("retained-database")).unwrap();
        std::fs::write(&before.database, b"source identity").unwrap();
        let replacement = Binding::at(&before.database, &before.archive, "aabb".into()).unwrap();
        assert_ne!(replacement, before);
        assert!(snapshot.validate(&replacement).is_err());
    }

    #[test]
    fn store_round_trip_holds_exclusive_ownership_until_drop() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let store = Store::open(binding.clone()).unwrap();
        assert!(store.load().unwrap().is_none());
        store.save(&debt(Instant::now())).unwrap();
        assert_eq!(store.load().unwrap().unwrap().after, 432);
        assert_eq!(Store::open(binding.clone()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(!binding.archive.join(".git").exists());
        drop(store);
        assert_eq!(Store::open(binding).unwrap().load().unwrap().unwrap().retries.len(), 2);
    }

    #[test]
    fn oversized_and_corrupt_checkpoint_reads_leave_bytes_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(binding(&temp)).unwrap();
        for bytes in [b"{corrupt".to_vec(), vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1]] {
            std::fs::write(&store.path, &bytes).unwrap();
            assert!(store.load().is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_sidecars_and_replaced_lock_do_not_modify_foreign_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let path = binding.path().unwrap();
        let foreign = temp.path().join("foreign");
        std::fs::write(&foreign, b"retained foreign bytes").unwrap();
        std::os::unix::fs::symlink(&foreign, &path).unwrap();
        assert!(Store::open(binding.clone()).is_err());
        std::fs::rename(&path, temp.path().join("retained-symlink")).unwrap();
        std::fs::hard_link(&foreign, &path).unwrap();
        assert!(Store::open(binding.clone()).is_err());
        std::fs::rename(&path, temp.path().join("retained-hardlink")).unwrap();
        let store = Store::open(binding.clone()).unwrap();
        std::fs::rename(&store.lock_path, temp.path().join("retained-lock")).unwrap();
        std::fs::write(&store.lock_path, b"new lock identity").unwrap();
        assert!(store.save(&debt(Instant::now())).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"retained foreign bytes");
        assert_eq!(std::fs::read(&store.lock_path).unwrap(), b"new lock identity");
    }

    // Real file-backed DB/Git cases run in separate processes because promotion
    // admission and archive fences are process-global. Never skip the body.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_AGENT_CHECKPOINT_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut log = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().unwrap();
        log.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        log.read_to_string(&mut output).unwrap();
        assert!(!expired && status.success() && output.contains("1 passed; 0 failed"), "{name}: timeout={expired}, {output}");
        true
    }

    fn with_mailbox(test: impl FnOnce(&Cx, &DbPool, &Config, &Path)) {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("mail.sqlite3")),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            std::fs::create_dir(&config.storage_root).unwrap();
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                warmup_connections: 0,
                ..Default::default()
            }).unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(101, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts, registration_token) VALUES(101, 101, 'BlueLake', 'test', 'test', 'checkpoint', 1000000, 2000000, 'never-checkpoint-this-token')").unwrap();
            drop(conn);
            test(&cx, &pool, &config, temp.path());
        });
    }

    fn profile(config: &Config) -> serde_json::Value {
        let relative = "projects/project/agents/BlueLake/profile.json";
        let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(relative)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        assert!(!String::from_utf8_lossy(&bytes).contains("never-checkpoint-this-token"));
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn fresh_cursor_restores_deferred_id_and_publishes_latest_deregistration() {
        with_mailbox(|cx, pool, config, _| {
            let stop = AtomicBool::new(false);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            for id in 1_i64..=40 {
                conn.execute_sync(
                    "INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(?, 101, ?, 'test', 'test', '', 1000000, 2000000)",
                    &[id.into(), format!("Agent{id}").into()],
                ).unwrap();
            }
            drop(conn);
            // Establish a real healthy roster larger than one scan page.
            // Without checkpoint restoration, a fresh cursor only visits IDs
            // 1..32 and cannot repair the lost lifecycle write for ID 101.
            let mut seeded = AgentReconcileCursor::default();
            let mut repaired = 0;
            for _ in 0..11 {
                repaired += reconcile_agent_batch(cx, pool, config, &mut seeded, &stop).unwrap().repaired;
            }
            assert_eq!(repaired, 41);
            let mut first = AgentReconcileCursor::default();
            assert_eq!(first.reconcile_checkpointed(cx, pool, config, &stop).unwrap().unchanged, 32);
            assert_eq!(first.after, 32);
            outcome(block_on(mcp_agent_mail_db::queries::set_agent_retired_at(cx, pool, 101, Some(123_456_789)))).unwrap();
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let fence = std::thread::spawn(move || {
                crate::with_archive_snapshot_publication_fence(|| {
                    held_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(15))
                })
            });
            held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let report = first.reconcile_checkpointed(cx, pool, config, &stop);
            let _ = release_tx.send(());
            assert!(fence.join().unwrap().is_ok(), "repair waited on the publication fence");
            let report = report.unwrap();
            assert_eq!((report.scanned, report.repaired, report.deferred_transient, report.retry_len), (9, 0, 9, 9));
            let path = first.retries.persistence.store.as_ref().unwrap().path.clone();
            let durable: Snapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(durable.retries.iter().map(|retry| retry.id).collect::<Vec<_>>(), [33, 34, 35, 36, 37, 38, 39, 40, 101]);
            assert_eq!(durable.after, 101);
            drop(first); // releases the actual OS lock, not just the retry payload

            // Change source lifecycle without delivering its archive write.
            outcome(block_on(mcp_agent_mail_db::queries::deregister_agent(cx, pool, 101, 234_567_891))).unwrap();
            let mut restarted = AgentReconcileCursor::default();
            let result = restarted.reconcile_checkpointed(cx, pool, config, &stop).unwrap();
            assert_eq!((result.repaired, result.deferred, result.retry_len), (1, 0, 0));
            assert_eq!(profile(config)["deregistered_at"], mcp_agent_mail_db::micros_to_iso(234_567_891));
            assert_eq!(profile(config)["contact_policy"], "block_all");
            assert_eq!(restarted.retries.persistence.store.as_ref().unwrap().load().unwrap().unwrap().retries.len(), 0);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn.query_sync("SELECT registration_token FROM agents WHERE id=101", &[]).unwrap();
            assert_eq!(rows[0].get_named::<String>("registration_token").unwrap(), "never-checkpoint-this-token");
        });
    }

    #[test]
    fn competing_cursor_defers_without_consuming_work_and_cancelled_cursor_creates_no_store() {
        with_mailbox(|cx, pool, config, _| {
            let mut cancelled = AgentReconcileCursor::default();
            let stop = AtomicBool::new(true);
            assert!(cancelled.reconcile_checkpointed(cx, pool, config, &stop).unwrap().interrupted);
            assert!(cancelled.retries.persistence.store.is_none());
            stop.store(false, Ordering::Release);
            let mut owner = AgentReconcileCursor::default();
            assert_eq!(owner.reconcile_checkpointed(cx, pool, config, &stop).unwrap().repaired, 1);
            let bytes = std::fs::read(owner.retries.persistence.store.as_ref().unwrap().path.clone()).unwrap();
            let mut contender = AgentReconcileCursor::default();
            let error = contender.reconcile_checkpointed(cx, pool, config, &stop).unwrap_err();
            assert_eq!(error, "agent reconciliation deferred: checkpoint is owned by another worker");
            assert_eq!(contender.after, 0);
            assert!(contender.retries.entries.is_empty());
            assert_eq!(std::fs::read(owner.retries.persistence.store.as_ref().unwrap().path.clone()).unwrap(), bytes);
            drop(owner);
            assert_eq!(contender.reconcile_checkpointed(cx, pool, config, &stop).unwrap().unchanged, 1);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn query_only_and_foreign_sources_create_no_checkpoint_or_archive() {
        with_mailbox(|cx, pool, config, scratch| {
            let path = Binding::capture(cx, pool, config).unwrap().path().unwrap();
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            }).unwrap();
            let mut cursor = AgentReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let error = cursor.reconcile_checkpointed(cx, &readonly, config, &stop).unwrap_err();
            assert!(error.contains("query-only snapshots"), "{error}");
            assert!(!path.exists() && !path.with_extension("json.lock").exists());
            assert!(!config.storage_root.join("projects").exists());

            let foreign = scratch.join("foreign.sqlite3");
            std::fs::write(&foreign, b"unrelated source").unwrap();
            let other = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&foreign),
                ..config.clone()
            };
            let error = cursor.reconcile_checkpointed(cx, pool, &other, &stop).unwrap_err();
            assert!(error.contains("not the configured live database"), "{error}");
            assert_eq!(std::fs::read(foreign).unwrap(), b"unrelated source");
            assert!(!path.exists() && !path.with_extension("json.lock").exists());
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    #[test]
    fn checkpoint_io_failure_never_reloads_older_progress_over_live_retry_debt() {
        with_mailbox(|cx, pool, config, scratch| {
            let stop = AtomicBool::new(false);
            let mut cursor = AgentReconcileCursor::default();
            assert_eq!(cursor.reconcile_checkpointed(cx, pool, config, &stop).unwrap().repaired, 1);
            let path = cursor.retries.persistence.store.as_ref().unwrap().path.clone();
            std::fs::rename(&path, scratch.join("old-checkpoint")).unwrap();
            std::fs::create_dir(&path).unwrap();
            // A retained debt below a completed watermark must survive both a
            // failed save and reappearance of the OLD valid checkpoint.
            cursor.retries.defer(999_999, Instant::now());
            cursor.retries.defer(999_999, Instant::now() + MAX_BACKOFF);
            let report = cursor.reconcile_checkpointed(cx, pool, config, &stop).unwrap();
            assert_eq!(report.retry_len, 1);
            assert!(cursor.retries.persistence.warned);
            std::fs::rename(&path, scratch.join("retained-checkpoint-obstruction")).unwrap();
            std::fs::rename(scratch.join("old-checkpoint"), &path).unwrap();
            let report = cursor.reconcile_checkpointed(cx, pool, config, &stop).unwrap();
            assert_eq!(report.retry_len, 1);
            assert!(!cursor.retries.persistence.warned);
            let saved = cursor.retries.persistence.store.as_ref().unwrap().load().unwrap().unwrap();
            assert_eq!(saved.retries[0].id, 999_999);
        });
    }
}
