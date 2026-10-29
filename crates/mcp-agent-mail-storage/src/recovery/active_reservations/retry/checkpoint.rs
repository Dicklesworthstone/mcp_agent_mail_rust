//! Durable, source-bound scheduling for active-lease recovery.
//!
//! A checkpoint records IDs and scan positions, never an ACTIVE payload. The
//! original reconciler still decides whether each lease may be published. One
//! cursor owns the sidecar lock until it is dropped or rebound: alternating
//! independent in-memory cursors must not overwrite each other's retry debt.

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
    ActiveReservationCursor, ActiveReservationReport, checkpoint, outcome,
    reconcile_active_reservations, source_error, validate_source,
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
            return Err(invalid(
                "checkpoint source or archive has an invalid file type",
            ));
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

    // Called only under database-promotion admission and after source binding
    // and query-only validation. Both file replacement and in-place logical
    // generation changes invalidate the hints; ordinary writes do not.
    fn capture(cx: &Cx, pool: &DbPool, config: &Config) -> Result<Self, String> {
        let conn = outcome(block_on(pool.acquire(cx)))?;
        let rows = conn
            .query_sync(
                "SELECT generation_id FROM db_identity WHERE singleton = 0",
                &[],
            )
            .map_err(source_error)?;
        let [row] = rows.as_slice() else {
            return Err("active reservation checkpoint requires one database generation".into());
        };
        let generation = row
            .get_named::<String>("generation_id")
            .map_err(source_error)?;
        drop(conn);
        Self::at(
            Path::new(pool.sqlite_path()),
            &config.storage_root,
            generation,
        )
        .map_err(|error| error.to_string())
    }

    fn path(&self) -> io::Result<PathBuf> {
        let parent = self
            .database
            .parent()
            .ok_or_else(|| invalid("source has no parent"))?;
        let mut digest = Sha256::new();
        digest.update(self.database.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(self.archive.as_os_str().as_encoded_bytes());
        Ok(parent.join(format!(
            ".am-active-reservation-reconcile-{}.json",
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
    retry_first: bool,
    retries: Vec<Retry>,
}

impl Snapshot {
    fn capture(
        binding: Binding,
        cursor: &ActiveReservationCursor,
        now: Instant,
        wall: i64,
    ) -> Self {
        Self {
            version: VERSION,
            binding,
            saved_at_us: wall,
            after: cursor.after,
            ceiling: cursor.ceiling,
            retry_first: cursor.retry_first,
            retries: cursor
                .retries
                .entries
                .iter()
                .map(|entry| Retry {
                    id: entry.id,
                    attempts: entry.attempts,
                    wait_us: u64::try_from(
                        entry
                            .ready_at
                            .saturating_duration_since(now)
                            .min(MAX_BACKOFF)
                            .as_micros(),
                    )
                    .unwrap_or(u64::MAX),
                })
                .collect(),
        }
    }

    fn validate(&self, binding: &Binding) -> io::Result<()> {
        if self.version != VERSION || &self.binding != binding {
            return Err(invalid(
                "checkpoint version or source/archive generation differs",
            ));
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

    fn restore(self, cursor: &mut ActiveReservationCursor, now: Instant, wall: i64) {
        let elapsed = u64::try_from(wall.saturating_sub(self.saved_at_us)).unwrap_or(0);
        cursor.after = self.after;
        cursor.ceiling = self.ceiling;
        cursor.retry_first = self.retry_first;
        cursor.retries.entries = self
            .retries
            .into_iter()
            .map(|retry| Entry {
                id: retry.id,
                attempts: retry.attempts,
                ready_at: now + Duration::from_micros(retry.wait_us.saturating_sub(elapsed)),
            })
            .collect();
        // The caller's live pool identity and lock ownership are never restored
        // from the file. A backwards clock can delay at most MAX_BACKOFF.
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
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
        }
        let lock = options.open(&lock_path)?;
        FileExt::try_lock_exclusive(&lock).map_err(|error| {
            // Windows reports ERROR_LOCK_VIOLATION, which std does not map to
            // WouldBlock. A live owner must defer repair, not select the
            // memory-only fallback used for unavailable checkpoint storage.
            if error
                .raw_os_error()
                .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
            {
                io::Error::new(io::ErrorKind::WouldBlock, error)
            } else {
                error
            }
        })?;
        let store = Self {
            binding,
            path,
            lock_path,
            lock,
        };
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
        let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &self.path, MAX_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
        snapshot.validate(&self.binding)?;
        Ok(Some(snapshot))
    }

    fn save(&self, cursor: &ActiveReservationCursor) -> io::Result<()> {
        self.check_lock()?;
        check_path(&self.path)?;
        let snapshot = Snapshot::capture(
            self.binding.clone(),
            cursor,
            Instant::now(),
            mcp_agent_mail_db::now_micros(),
        );
        snapshot.validate(&self.binding)?;
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| invalid(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(invalid("checkpoint exceeds its byte limit"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("checkpoint has no parent"))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".am-active-reservation-")
            .tempfile_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn warn_unavailable(state: &mut Persistence, reason: &str) {
    if !state.warned {
        state.warned = true;
        tracing::warn!(target: "maintenance", event = "active_reservation_checkpoint_unavailable",
            reason, "active lease repair continues in memory; restart progress is not durable");
    }
}

impl ActiveReservationCursor {
    /// Run the ordinary create-only reconciler with restart-safe scan/retry hints.
    ///
    /// Hints are atomically checkpointed beside the live database, bound to its
    /// physical and logical generation and configured archive root. They never
    /// carry lease payloads or authorize a write. The original per-pass limits,
    /// source rechecks, release/expiry rules and no-clobber checks still apply.
    /// This cursor retains one nonblocking sidecar lock until dropped/rebound.
    /// Unavailable/corrupt hints do not disable repair or replace newer in-memory
    /// debt. Another checkpoint owner defers the pass without consuming work.
    ///
    /// # Errors
    /// Returns source/admission errors from the normal reconciler. Checkpoint
    /// ownership is a transient admission error; other checkpoint I/O failures
    /// are logged and leave the cursor memory-only until a later save succeeds.
    pub fn reconcile_checkpointed(
        &mut self,
        cx: &Cx,
        pool: &DbPool,
        config: &Config,
        stop: &AtomicBool,
    ) -> Result<ActiveReservationReport, String> {
        if checkpoint(cx, stop).is_err() || corruption_circuit_breaker().is_tripped() {
            return reconcile_active_reservations(cx, pool, config, self, stop);
        }
        let (scope, captured) = {
            let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or(
                "active reservation repair deferred: recovery promotion or admission contention",
            )?;
            validate_source(cx, pool, config)?;
            (
                pool.sqlite_identity_key(),
                Binding::capture(cx, pool, config),
            )
        };
        let rebind = self.identity != scope
            || captured.as_ref().is_ok_and(|binding| {
                self.retries
                    .persistence
                    .binding
                    .as_ref()
                    .is_some_and(|known| known != binding)
            });
        if rebind {
            *self = Self {
                identity: scope.clone(),
                ..Self::default()
            };
            self.retries.persistence.restore_eligible = true;
        }
        let mut state = std::mem::take(&mut self.retries.persistence);
        let binding = match captured {
            Ok(binding) => Some(binding),
            Err(error) => {
                warn_unavailable(&mut state, &error);
                None
            }
        };
        if let Some(binding) = &binding {
            state.binding = Some(binding.clone());
            if let Some(store) = &state.store
                && let Err(error) = store.check_lock()
            {
                state.store = None;
                warn_unavailable(&mut state, &error.to_string());
            }
            if state.store.is_none() {
                match Store::open(binding.clone()) {
                    Ok(store) => state.store = Some(store),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.retries.persistence = state;
                        return Err("active reservation repair deferred: checkpoint is owned by another worker".into());
                    }
                    Err(error) => warn_unavailable(&mut state, &error.to_string()),
                }
            }
        }
        // Only a fresh/rebound cursor may import a disk snapshot. A cursor
        // already used by the in-memory API, or whose earlier save failed,
        // must never replace its newer retry debt with an older checkpoint.
        if !state.restore_attempted
            && state.restore_eligible
            && let Some(store) = &state.store
        {
            match store.load() {
                Ok(Some(snapshot)) => {
                    snapshot.restore(self, Instant::now(), mcp_agent_mail_db::now_micros())
                }
                Ok(None) => {}
                Err(error) => warn_unavailable(&mut state, &error.to_string()),
            }
        }
        state.restore_attempted = true;
        let result = reconcile_active_reservations(cx, pool, config, self, stop);
        // Never attach old progress to a replacement source. Reacquire only
        // nonblocking promotion admission; cancellation may replay the last
        // durable pass on restart, but cannot advance past unsaved retry debt.
        if self.identity == scope
            && checkpoint(cx, stop).is_ok()
            && !corruption_circuit_breaker().is_tripped()
            && let (Some(expected), Some(store)) = (&binding, &state.store)
        {
            let saved = (|| {
                {
                    let _admission = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
                        .ok_or_else(|| {
                        "checkpoint save deferred by recovery promotion".to_string()
                    })?;
                    if pool.sqlite_identity_key() != scope
                        || Binding::capture(cx, pool, config)? != *expected
                    {
                        return Err("checkpoint source changed during active lease repair".into());
                    }
                }
                // Promotion need not wait for hint fsync. If it replaces
                // the source now, this snapshot still names the OLD
                // binding and cannot be restored into that replacement.
                store.save(self).map_err(|error| error.to_string())
            })();
            match saved {
                Ok(()) => state.warned = false,
                Err(error) => warn_unavailable(&mut state, &error),
            }
        }
        if self.identity == scope {
            self.retries.persistence = state;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn binding(temp: &tempfile::TempDir) -> Binding {
        let db = temp.path().join("mail.sqlite3");
        std::fs::write(&db, b"fixture database identity").unwrap();
        let archive = temp.path().join("archive");
        std::fs::create_dir(&archive).unwrap();
        Binding::at(&db, &archive, "aabb".into()).unwrap()
    }

    fn cursor(now: Instant) -> ActiveReservationCursor {
        let mut cursor = ActiveReservationCursor {
            identity: "live-only-pool-identity".into(),
            after: 432,
            ceiling: Some(480),
            retry_first: true,
            ..Default::default()
        };
        assert!(cursor.retries.defer(401, now));
        assert!(cursor.retries.defer(401, now));
        assert!(cursor.retries.defer(402, now));
        cursor
    }

    #[test]
    fn snapshot_round_trip_keeps_fifo_scan_progress_and_remaining_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        let original = cursor(now);
        let bytes = serde_json::to_vec(&Snapshot::capture(
            binding.clone(),
            &original,
            now,
            1_000_000,
        ))
        .unwrap();
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        snapshot.validate(&binding).unwrap();
        let mut resumed = ActiveReservationCursor::default();
        snapshot.restore(&mut resumed, now, 1_500_000);
        assert_eq!(
            (resumed.after, resumed.ceiling, resumed.retry_first),
            (432, Some(480), true)
        );
        assert!(
            resumed.identity.is_empty(),
            "hints must not restore live pool authority"
        );
        assert_eq!(resumed.retries.due(now), [402]);
        assert_eq!(
            resumed.retries.due(now + Duration::from_millis(500)),
            [401, 402]
        );
        assert_eq!(resumed.retries.entries[0].attempts, 1);
        assert!(resumed.retries.persistence.store.is_none());
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("path_pattern") && !text.contains("expires_ts"));
    }

    #[test]
    fn clock_reversal_cannot_extend_backoff_and_downtime_makes_old_debt_due() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        for wall in [1, 1_500_000, 40_000_000, i64::MAX] {
            let original = cursor(now);
            let snapshot = Snapshot::capture(binding.clone(), &original, now, 2_000_000);
            let mut resumed = ActiveReservationCursor::default();
            snapshot.restore(&mut resumed, now, wall);
            assert!(
                resumed
                    .retries
                    .entries
                    .iter()
                    .all(|entry| entry.ready_at <= now + MAX_BACKOFF)
            );
            if wall >= 40_000_000 {
                assert_eq!(resumed.retries.due(now), [401, 402]);
            } else {
                assert_eq!(resumed.retries.due(now), [402]);
            }
        }
    }

    #[test]
    fn invalid_snapshots_cannot_hide_or_duplicate_scheduling_debt() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        for case in [
            "version",
            "time",
            "position",
            "ceiling",
            "id",
            "duplicate",
            "delay",
            "overflow",
        ] {
            let mut snapshot = Snapshot::capture(binding.clone(), &cursor(now), now, 1_000_000);
            match case {
                "version" => snapshot.version += 1,
                "time" => snapshot.saved_at_us = 0,
                "position" => snapshot.after = -1,
                "ceiling" => snapshot.ceiling = Some(431),
                "id" => snapshot.retries[0].id = 0,
                "duplicate" => snapshot.retries[1].id = 401,
                "delay" => snapshot.retries[0].wait_us = u64::MAX,
                "overflow" => {
                    snapshot.retries = (1..=MAX_RETRY_IDS + 1)
                        .map(|id| Retry {
                            id: i64::try_from(id).unwrap(),
                            attempts: 0,
                            wait_us: 0,
                        })
                        .collect()
                }
                _ => unreachable!(),
            }
            assert!(snapshot.validate(&binding).is_err(), "{case}");
        }
        let mut full = ActiveReservationCursor::default();
        for id in 1..=i64::try_from(MAX_RETRY_IDS).unwrap() {
            assert!(full.retries.defer(id, now));
        }
        let snapshot = Snapshot::capture(binding.clone(), &full, now, 1_000_000);
        snapshot.validate(&binding).unwrap();
        assert!(u64::try_from(serde_json::to_vec(&snapshot).unwrap().len()).unwrap() < MAX_BYTES);
    }

    #[test]
    fn binding_survives_writes_but_not_replacement_generation_or_archive_changes() {
        let temp = tempfile::tempdir().unwrap();
        let original = binding(&temp);
        std::fs::write(
            &original.database,
            b"ordinary write changes length and mtime",
        )
        .unwrap();
        assert_eq!(
            Binding::at(&original.database, &original.archive, "aabb".into()).unwrap(),
            original
        );
        let changed_generation =
            Binding::at(&original.database, &original.archive, "ccdd".into()).unwrap();
        let snapshot =
            Snapshot::capture(original.clone(), &cursor(Instant::now()), Instant::now(), 1);
        assert!(snapshot.validate(&changed_generation).is_err());
        let other_root = temp.path().join("other-archive");
        std::fs::create_dir(&other_root).unwrap();
        let other_archive = Binding::at(&original.database, &other_root, "aabb".into()).unwrap();
        assert_ne!(original.path().unwrap(), other_archive.path().unwrap());
        assert!(snapshot.validate(&other_archive).is_err());
        std::fs::rename(
            &original.database,
            temp.path().join("retained-old-database"),
        )
        .unwrap();
        std::fs::write(&original.database, b"replacement").unwrap();
        let replacement =
            Binding::at(&original.database, &original.archive, "aabb".into()).unwrap();
        assert!(snapshot.validate(&replacement).is_err());
    }

    #[test]
    fn store_owns_the_lock_until_drop_and_restores_the_atomically_saved_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        #[cfg(windows)]
        assert!(matches!(
            binding.database.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        let store = Store::open(binding.clone()).unwrap();
        assert!(store.load().unwrap().is_none());
        let mut cursor = cursor(Instant::now());
        store.save(&cursor).unwrap();
        let bytes = std::fs::read(&store.path).unwrap();
        assert_eq!(
            Store::open(binding.clone()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        let snapshot = store.load().unwrap().unwrap();
        assert_eq!(
            (snapshot.after, snapshot.ceiling, snapshot.retries.len()),
            (432, Some(480), 2)
        );
        cursor.after = 433;
        store.save(&cursor).unwrap();
        let updated = std::fs::read(&store.path).unwrap();
        drop(store);
        let reopened = Store::open(binding).unwrap();
        assert_eq!(std::fs::read(&reopened.path).unwrap(), updated);
        let restored = reopened.load().unwrap().unwrap();
        assert_eq!(restored.after, 433);
        assert_eq!(restored.retries[0].id, 401);
    }

    #[test]
    fn oversized_corrupt_or_unpublishable_checkpoints_do_not_replace_saved_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let store = Store::open(binding).unwrap();
        store.save(&cursor(Instant::now())).unwrap();
        let before = std::fs::read(&store.path).unwrap();
        let mut invalid_cursor = cursor(Instant::now());
        invalid_cursor.after = -1;
        assert!(store.save(&invalid_cursor).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), before);
        std::fs::write(&store.path, b"{broken").unwrap();
        assert!(store.load().is_err());
        File::create(&store.path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(store.load().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_sidecars_and_replaced_lock_names_never_authorize_checkpoint_writes() {
        use std::os::unix::fs::symlink;
        for hard in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let binding = binding(&temp);
            let outside = temp.path().join("retained-evidence");
            std::fs::write(&outside, b"do not modify").unwrap();
            let path = binding.path().unwrap();
            if hard {
                std::fs::hard_link(&outside, &path).unwrap();
            } else {
                symlink(&outside, &path).unwrap();
            }
            assert!(Store::open(binding).is_err());
            assert_eq!(std::fs::read(outside).unwrap(), b"do not modify");
        }
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(binding(&temp)).unwrap();
        std::fs::rename(&store.lock_path, temp.path().join("retained-lock")).unwrap();
        std::fs::write(&store.lock_path, b"another owner's lock").unwrap();
        assert!(store.save(&cursor(Instant::now())).is_err());
        assert!(!store.path.exists());
        assert_eq!(
            std::fs::read(&store.lock_path).unwrap(),
            b"another owner's lock"
        );
    }

    // These tests deliberately hold the real process-wide publication fence.
    // Isolate it from other libtests and bound the child's entire lifetime.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_ACTIVE_CHECKPOINT_CHILD";
        let thread = std::thread::current();
        let name = thread.name().unwrap();
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !expired && output.status.success(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        true
    }

    fn fixture(test: impl FnOnce(&Cx, &DbPool, &Config)) {
        if isolated() {
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            std::fs::create_dir(&config.storage_root).unwrap();
            let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let cx = Cx::for_testing();
            let conn = outcome(block_on(pool.acquire(&cx))).unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw(
                "INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')",
            )
            .unwrap();
            let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            for id in 401..=480 {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'src/{id}.rs', 1, 'checkpoint-test', 1000000, {expires}, NULL)")).unwrap();
            }
            drop(conn);
            test(&cx, &pool, &config);
        });
    }

    #[test]
    fn gh369_competing_checkpoint_owner_defers_active_repair_without_memory_fallback() {
        fixture(|cx, pool, config| {
            let stop = AtomicBool::new(false);
            let owner = Store::open(Binding::capture(cx, pool, config).unwrap()).unwrap();
            owner.save(&ActiveReservationCursor::default()).unwrap();
            let saved = std::fs::read(&owner.path).unwrap();
            let mut contender = ActiveReservationCursor::default();
            for _ in 0..3 {
                let error = contender
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap_err();
                assert_eq!(
                    error,
                    "active reservation repair deferred: checkpoint is owned by another worker"
                );
                assert_eq!(contender.after, 0);
                assert!(!contender.retries.persistence.restore_attempted);
                assert!(!contender.retries.persistence.warned);
                assert!(!config.storage_root.join("projects").exists());
                assert_eq!(std::fs::read(&owner.path).unwrap(), saved);
            }
            drop(owner);
            let resumed = contender
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(resumed.repaired, 4);
            let relative = "projects/project/file_reservations/id-401-gaabb.json";
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            let entry = tree.get_path(Path::new(relative)).unwrap();
            assert_eq!(
                repo.find_blob(entry.id()).unwrap().content(),
                std::fs::read(config.storage_root.join(relative)).unwrap()
            );
            let store = contender.retries.persistence.store.as_ref().unwrap();
            assert_eq!(store.load().unwrap().unwrap().after, contender.after);
        });
    }

    #[test]
    fn restart_and_deferred_lock_owner_restore_real_debt_but_reread_release_and_generation() {
        fixture(|cx, pool, config| {
            let stop = AtomicBool::new(false);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let owner = std::thread::spawn(move || {
                crate::with_archive_snapshot_publication_fence(|| {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10))
                })
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut original = ActiveReservationCursor::default();
            let first = original.reconcile_checkpointed(cx, pool, config, &stop);
            // Always release the global fence before assertions could unwind.
            let _ = release_tx.send(());
            assert!(
                owner.join().unwrap().is_ok(),
                "recovery waited for fence expiry"
            );
            let first = first.unwrap();
            assert_eq!(
                (first.scanned, first.deferred_transient, first.retry_len),
                (32, 32, 32)
            );
            assert_eq!((original.after, original.ceiling), (432, Some(480)));
            let path = Binding::capture(cx, pool, config).unwrap().path().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let mut resumed = ActiveReservationCursor::default();
            let error = resumed
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap_err();
            assert!(error.contains("checkpoint is owned by another worker"));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            drop(original); // The next call must restore even after earlier lock contention.
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_raw("UPDATE file_reservations SET reason='renewed after restart', expires_ts=expires_ts+1000000 WHERE id=401").unwrap();
            conn.execute_raw("INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(402, 5000000)").unwrap();
            drop(conn);
            let report = resumed
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(report.deferred, 0);
            assert!(report.attempted <= 4 && report.scanned <= 32);
            assert!(
                resumed.after > 432,
                "the persisted discovery position was lost"
            );
            let relative = "projects/project/file_reservations/id-401-gaabb.json";
            let lease_bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&lease_bytes).unwrap();
            assert_eq!(value["reason"], "renewed after restart");
            assert!(
                !config
                    .storage_root
                    .join("projects/project/file_reservations/id-402-gaabb.json")
                    .exists()
            );
            let repo = git2::Repository::open(&config.storage_root).unwrap();
            let tree = repo.head().unwrap().peel_to_tree().unwrap();
            let entry = tree.get_path(Path::new(relative)).unwrap();
            assert_eq!(repo.find_blob(entry.id()).unwrap().content(), lease_bytes);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert_eq!(
                conn.query_sync("SELECT COUNT(*) FROM file_reservations", &[])
                    .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                80
            );
            assert_eq!(
                conn.query_sync(
                    "SELECT released_ts FROM file_reservation_releases WHERE reservation_id=402",
                    &[]
                )
                .unwrap()[0]
                    .get_as::<i64>(0)
                    .unwrap(),
                5_000_000
            );
            conn.execute_raw("UPDATE db_identity SET generation_id='ccdd' WHERE singleton=0")
                .unwrap();
            drop(conn);
            let rebound = resumed
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(rebound.deferred, 0);
            assert!(
                resumed.after < 432,
                "a new logical generation must start its own scan"
            );
            assert!(
                config
                    .storage_root
                    .join("projects/project/file_reservations/id-401-gccdd.json")
                    .exists()
            );
            assert_eq!(
                std::fs::read(config.storage_root.join(relative)).unwrap(),
                lease_bytes
            );
            let saved: Snapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved.binding.generation, "ccdd");
        });
    }

    #[test]
    fn precancelled_checkpointed_repair_does_not_create_hints_or_consume_candidates() {
        fixture(|cx, pool, config| {
            let stop = AtomicBool::new(true);
            let mut cursor = ActiveReservationCursor::default();
            let report = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert!(report.interrupted);
            assert_eq!(
                (report.scanned, cursor.after, cursor.retries.len()),
                (0, 0, 0)
            );
            let path = Binding::capture(cx, pool, config).unwrap().path().unwrap();
            assert!(!path.exists() && !path.with_extension("json.lock").exists());
            assert!(!config.storage_root.join("projects").exists());
            assert!(stop.load(Ordering::Acquire));
        });
    }
}
