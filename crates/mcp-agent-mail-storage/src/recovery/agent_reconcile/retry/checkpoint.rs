//! Restart-safe scheduling for agent lifecycle archive recovery.
//!
//! The checkpoint contains IDs, roster position, bounded retry delays and
//! conflict fingerprints, never a profile or authorization token. The cursor
//! owns one nonblocking sidecar lock for its lifetime, so independent workers
//! cannot overwrite retry debt or restore conflicting scheduling hints.

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
use super::{ConflictEntry, Entry, MAX_BACKOFF, MAX_CONFLICT_IDS, MAX_RETRY_IDS};

const VERSION: u32 = 2;
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
        #[cfg(windows)]
        let source = mcp_agent_mail_core::disk::open_regular_file_no_follow(&database)?;
        #[cfg(windows)]
        let metadata = source.metadata()?;
        #[cfg(not(windows))]
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
        #[cfg(windows)]
        let file_identity = {
            let (volume, index) = mcp_agent_mail_core::disk::windows_file_identity(&source)?;
            let created = created.ok_or_else(|| invalid("source has no stable file identity"))?;
            // NTFS can preserve creation time when a different file replaces
            // this path. Bind to the opened file's identity as well.
            format!("{volume}:{index}:{created:?}")
        };
        #[cfg(not(any(unix, windows)))]
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
    fn capture(cx: &Cx, pool: &DbPool, config: &Config, stop: &AtomicBool) -> Result<Self, String> {
        checkpoint(cx, stop).map_err(|error| error.to_string())?;
        let conn = outcome(block_on(pool.acquire(cx)))?;
        checkpoint(cx, stop).map_err(|error| error.to_string())?;
        let mode = conn
            .query_sync("PRAGMA query_only", &[])
            .map_err(source_error)?;
        if mode.first().and_then(|row| row.get_as::<i64>(0).ok()) != Some(0) {
            return Err("query-only snapshots cannot authorize agent archive repair".into());
        }
        // Fresh mailboxes need not have any reservations. Only an actually
        // empty identity relation permits initializing a generation here.
        let select = "SELECT singleton, generation_id FROM db_identity LIMIT 2";
        let mut rows = conn.query_sync(select, &[]).map_err(source_error)?;
        if rows.is_empty() {
            checkpoint(cx, stop).map_err(|error| error.to_string())?;
            if corruption_circuit_breaker().is_tripped() {
                return Err(
                    "agent checkpoint identity initialization refused: corruption breaker open"
                        .into(),
                );
            }
            if mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .load()
                >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
            {
                return Err(
                    "agent checkpoint identity initialization deferred by critical disk pressure"
                        .into(),
                );
            }
            let token = mcp_agent_mail_core::setup::generate_token().map_err(|error| {
                format!("agent checkpoint identity initialization failed: {error}")
            })?;
            conn.execute_sync(
                "INSERT OR IGNORE INTO db_identity (singleton, generation_id) VALUES (0, ?)",
                &[mcp_agent_mail_db::sqlmodel_core::Value::Text(token)],
            )
            .map_err(source_error)?;
            // A racing reservation or recovery worker may have initialized
            // first. Bind only the stored winner, never our proposed token.
            rows = conn.query_sync(select, &[]).map_err(source_error)?;
        }
        let [row] = rows.as_slice() else {
            return Err("agent checkpoint requires one database generation".into());
        };
        if row.get_named::<i64>("singleton").map_err(source_error)? != 0 {
            return Err("agent checkpoint database generation has an invalid singleton".into());
        }
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
    conflicts: Vec<ConflictEntry>,
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
            conflicts: cursor.conflicts.entries.iter().cloned().collect(),
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
            || self.conflicts.len() > MAX_CONFLICT_IDS
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
        for conflict in &self.conflicts {
            if !conflict.is_valid() || !ids.insert(conflict.id) {
                return Err(invalid(
                    "checkpoint conflict IDs or fingerprints are invalid",
                ));
            }
        }
        Ok(())
    }

    fn matches_schedule_without_retries(&self, other: &Self) -> bool {
        self.version == other.version
            && self.binding == other.binding
            && self.retries.is_empty()
            && other.retries.is_empty()
            && self.after == other.after
            && self.ceiling == other.ceiling
            && self.next_lane_is_retry == other.next_lane_is_retry
            && self.conflicts == other.conflicts
    }

    fn restore(self, cursor: &mut AgentReconcileCursor, now: Instant, wall: i64) {
        let elapsed = u64::try_from(wall.saturating_sub(self.saved_at_us)).unwrap_or(0);
        cursor.after = self.after;
        cursor.ceiling = self.ceiling;
        cursor.next_lane_is_retry = self.next_lane_is_retry;
        cursor.retries.entries = self
            .retries
            .into_iter()
            .map(|retry| Entry {
                id: retry.id,
                attempts: retry.attempts,
                ready_at: now + Duration::from_micros(retry.wait_us.saturating_sub(elapsed)),
            })
            .collect();
        cursor.conflicts.entries = self.conflicts.into();
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
            #[cfg(windows)]
            {
                let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(path)?;
                if mcp_agent_mail_core::disk::windows_file_link_count(&file)? != 1 {
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
struct CheckpointWitness {
    // Keep the confirmed file open so a replacement cannot reuse its identity.
    file: File,
    digest: [u8; 32],
    metadata: std::fs::Metadata,
}

impl CheckpointWitness {
    fn capture(file: File, bytes: &[u8]) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if metadata.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
            return Err(invalid("checkpoint changed while its bytes were read"));
        }
        Ok(Self {
            file,
            digest: Sha256::digest(bytes).into(),
            metadata,
        })
    }

    fn matches_file(&self, file: &File) -> io::Result<bool> {
        let retained = self.file.metadata()?;
        let observed = file.metadata()?;
        if !retained.is_file()
            || !observed.is_file()
            || self.metadata.len() != observed.len()
            || self.metadata.modified()? != observed.modified()?
        {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Ok(retained.nlink() == 1
                && observed.nlink() == 1
                && (retained.dev(), retained.ino()) == (observed.dev(), observed.ino())
                && (self.metadata.ctime(), self.metadata.ctime_nsec())
                    == (observed.ctime(), observed.ctime_nsec()))
        }
        #[cfg(windows)]
        {
            Ok(
                mcp_agent_mail_core::disk::windows_file_link_count(&self.file)? == 1
                    && mcp_agent_mail_core::disk::windows_file_link_count(file)? == 1
                    && mcp_agent_mail_core::disk::windows_file_identity(&self.file)?
                        == mcp_agent_mail_core::disk::windows_file_identity(file)?,
            )
        }
        #[cfg(not(any(unix, windows)))]
        {
            // Without a stable open-file identity, retain ordinary publication.
            Ok(false)
        }
    }

    fn matches(&self, observed: &Self) -> io::Result<bool> {
        Ok(self.digest == observed.digest && self.matches_file(&observed.file)?)
    }
}

#[derive(Debug)]
struct Store {
    binding: Binding,
    path: PathBuf,
    lock_path: PathBuf,
    lock: File,
    // Readable bytes alone do not prove a previous rename was synced. Only
    // this store's successful durable publication can establish this witness.
    confirmed: Option<CheckpointWitness>,
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
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
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
            confirmed: None,
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
        #[cfg(windows)]
        {
            let named_file =
                mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.lock_path)?;
            if mcp_agent_mail_core::disk::windows_file_identity(&self.lock)?
                != mcp_agent_mail_core::disk::windows_file_identity(&named_file)?
            {
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
        self.read_snapshot()
            .map(|loaded| loaded.map(|(snapshot, _)| snapshot))
    }

    fn read_snapshot(&self) -> io::Result<Option<(Snapshot, CheckpointWitness)>> {
        use std::io::Read as _;

        self.check_lock()?;
        check_path(&self.path)?;
        let file = match mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_BYTES {
            return Err(invalid("checkpoint exceeds its byte limit"));
        }
        let mut bytes = Vec::new();
        (&file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(invalid("checkpoint grew beyond its byte limit"));
        }
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
        snapshot.validate(&self.binding)?;
        Ok(Some((snapshot, CheckpointWitness::capture(file, &bytes)?)))
    }

    fn save(&mut self, cursor: &AgentReconcileCursor) -> io::Result<()> {
        // Every failed save, including a failure AFTER rename, invalidates the
        // durability witness. A later matching read must not certify that an
        // earlier failed sync completed. A freshly opened store also writes once.
        let confirmed = self.confirmed.take();
        self.check_lock()?;
        check_path(&self.path)?;
        // Ordinary data writes keep this binding; physical replacement does not.
        if Binding::at(
            &self.binding.database,
            &self.binding.archive,
            self.binding.generation.clone(),
        )? != self.binding
        {
            return Err(invalid(
                "checkpoint source file changed during reconciliation",
            ));
        }
        let snapshot = Snapshot::capture(
            self.binding.clone(),
            cursor,
            Instant::now(),
            mcp_agent_mail_db::now_micros(),
        );
        snapshot.validate(&self.binding)?;
        // Reuse only this store's confirmed publication, with freshly parsed
        // bytes and a matching digest, metadata and retained file identity.
        // An in-place overwrite or identical replacement is not a sync receipt.
        // Retry delays remain on the ordinary save path so their wall-clock
        // anchor stays paired with the captured remaining wait.
        if snapshot.retries.is_empty()
            && let Some(confirmed) = confirmed
            && let Ok(Some((saved, observed))) = self.read_snapshot()
            && saved.matches_schedule_without_retries(&snapshot)
            && confirmed.matches(&observed).unwrap_or(false)
        {
            self.check_lock()?;
            check_path(&self.path)?;
            if let Ok(named) = mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.path)
                && confirmed.matches_file(&named).unwrap_or(false)
            {
                self.confirmed = Some(confirmed);
                return Ok(());
            }
        }
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| invalid(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(invalid("checkpoint exceeds its byte limit"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("checkpoint has no parent"))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".am-agent-reconcile-")
            .tempfile_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        #[cfg(any(unix, windows))]
        let synced = temporary.as_file().metadata()?;
        self.check_lock()?;
        check_path(&self.path)?;
        let published = temporary.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(test)]
        tests::after_checkpoint_persist()?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        #[cfg(any(unix, windows))]
        {
            self.confirm_publication(published, &bytes, &synced)
        }
        #[cfg(not(any(unix, windows)))]
        {
            drop(published);
            Ok(())
        }
    }

    #[cfg(any(unix, windows))]
    fn confirm_publication(
        &mut self,
        published: File,
        bytes: &[u8],
        synced: &std::fs::Metadata,
    ) -> io::Result<()> {
        let confirmed = CheckpointWitness::capture(published, bytes)?;
        if confirmed.metadata.len() != synced.len()
            || confirmed.metadata.modified()? != synced.modified()?
        {
            return Err(invalid("checkpoint changed after file sync"));
        }
        // Rename can change Unix ctime. Pin its post-publication value, then
        // verify actual bounded bytes and metadata before retaining a receipt.
        let Some((_, observed)) = self.read_snapshot()? else {
            return Err(invalid("published checkpoint disappeared"));
        };
        if !confirmed.matches(&observed)? {
            return Err(invalid("checkpoint changed after publication"));
        }
        self.check_lock()?;
        check_path(&self.path)?;
        let named = mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.path)?;
        if !confirmed.matches_file(&named)? {
            return Err(invalid("published checkpoint identity changed"));
        }
        self.confirmed = Some(confirmed);
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
    /// pairs its roster watermark with retry IDs and conflict fingerprints,
    /// bound to the configured database/root and physical/logical generation.
    /// It never contains profiles or tokens and does not relax the normal
    /// source or publication checks.
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
            let _selection = mcp_agent_mail_db::write_barrier::try_begin_write_activity().ok_or(
                "agent reconciliation deferred: recovery promotion or admission contention",
            )?;
            validate_source(pool, config)?;
            // Capture must succeed before sidecar mutation. In particular an
            // unreadable/query-only source is not merely a missing hint store.
            (
                pool.sqlite_identity_key(),
                Binding::capture(cx, pool, config, stop)?,
            )
        };
        // A new pool is not a new mailbox. Preserve unsaved retry debt, scan
        // progress, and the held checkpoint lock only when the independently
        // retained binding agrees with this freshly validated source. Updating
        // the process-local key also prevents the ordinary batch from resetting
        // the cursor. Unknown or changed bindings still take the rescan path.
        if self.source_identity != scope
            && self.retries.persistence.binding.as_ref() == Some(&captured)
        {
            self.source_identity.clone_from(&scope);
        }
        let rebind = self.source_identity != scope
            || self
                .retries
                .persistence
                .binding
                .as_ref()
                .is_some_and(|known| known != &captured);
        if rebind {
            *self = Self {
                source_identity: scope.clone(),
                ..Self::default()
            };
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
            return Err(format!(
                "agent checkpoint ownership changed; rescan required: {error}"
            ));
        }
        if state.store.is_none() {
            match Store::open(captured.clone()) {
                Ok(store) => state.store = Some(store),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.retries.persistence = state;
                    return Err(
                        "agent reconciliation deferred: checkpoint is owned by another worker"
                            .into(),
                    );
                }
                Err(error) => warn_unavailable(&mut state, &error.to_string()),
            }
        }
        if !state.restore_attempted
            && state.restore_eligible
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
            && let Some(store) = &mut state.store
        {
            let saved = (|| -> Result<(), String> {
                {
                    let _admission = mcp_agent_mail_db::write_barrier::try_begin_write_activity()
                        .ok_or("agent checkpoint save deferred by promotion")?;
                    validate_source(pool, config)?;
                    if Binding::capture(cx, pool, config, stop)? != captured {
                        return Err(
                            "agent checkpoint source generation changed during reconciliation"
                                .into(),
                        );
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

    std::thread_local! {
        static FAIL_AFTER_PERSIST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn after_checkpoint_persist() -> io::Result<()> {
        if FAIL_AFTER_PERSIST.replace(false) {
            Err(io::Error::other(
                "injected checkpoint directory sync failure",
            ))
        } else {
            Ok(())
        }
    }

    fn binding(temp: &tempfile::TempDir) -> Binding {
        let database = temp.path().join("mail.sqlite3");
        let archive = temp.path().join("archive");
        std::fs::write(&database, b"source identity").unwrap();
        std::fs::create_dir(&archive).unwrap();
        Binding::at(&database, &archive, "aabb".into()).unwrap()
    }

    #[cfg(windows)]
    fn match_creation_time(path: &Path, original: &File) {
        use std::os::windows::fs::FileTimesExt as _;

        let created = original.metadata().unwrap().created().unwrap();
        let replacement =
            mcp_agent_mail_core::disk::open_regular_file_read_write_no_follow(path).unwrap();
        replacement
            .set_times(std::fs::FileTimes::new().set_created(created))
            .unwrap();
        assert_eq!(replacement.metadata().unwrap().created().unwrap(), created);
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
        cursor.conflicts.remember(403, "a".repeat(64));
        cursor.conflicts.remember(404, "b".repeat(64));
        cursor
    }

    fn conflict_schedule() -> AgentReconcileCursor {
        let mut cursor = debt(Instant::now());
        cursor.retries.entries.clear();
        cursor
    }

    #[cfg(any(unix, windows))]
    fn file_identity(file: &File) -> (u64, u64) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = file.metadata().unwrap();
            (metadata.dev(), metadata.ino())
        }
        #[cfg(windows)]
        {
            mcp_agent_mail_core::disk::windows_file_identity(file).unwrap()
        }
    }

    #[test]
    fn snapshot_pairs_roster_position_fifo_debt_and_remaining_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        let bytes = serde_json::to_vec(&Snapshot::capture(
            binding.clone(),
            &debt(now),
            now,
            1_000_000,
        ))
        .unwrap();
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        snapshot.validate(&binding).unwrap();
        let mut resumed = AgentReconcileCursor::default();
        snapshot.restore(&mut resumed, now, 1_500_000);
        assert_eq!(
            (resumed.after, resumed.ceiling, resumed.next_lane_is_retry),
            (432, Some(480), true)
        );
        assert!(
            resumed.source_identity.is_empty(),
            "a hint cannot restore pool authority"
        );
        assert!(resumed.retries.persistence.store.is_none());
        assert_eq!(resumed.retries.ready_ids(now), [402]);
        assert_eq!(
            resumed.retries.ready_ids(now + Duration::from_millis(500)),
            [401, 402]
        );
        assert_eq!(resumed.retries.entries[0].attempts, 1);
        assert_eq!(resumed.conflicts.len(), 2);
        assert!(resumed.conflicts.matches(403, &"a".repeat(64)));
        assert!(resumed.conflicts.matches(404, &"b".repeat(64)));
        assert_eq!(
            resumed
                .conflicts
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [403, 404]
        );
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
            assert!(
                resumed
                    .retries
                    .entries
                    .iter()
                    .all(|entry| { entry.ready_at >= now && entry.ready_at <= now + MAX_BACKOFF })
            );
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
        for mutation in [
            "version",
            "time",
            "negative",
            "ceiling",
            "id",
            "duplicate",
            "wait",
            "count",
        ] {
            let mut snapshot = Snapshot::capture(binding.clone(), &debt(now), now, 1_000_000);
            match mutation {
                "version" => snapshot.version += 1,
                "time" => snapshot.saved_at_us = 0,
                "negative" => snapshot.after = -1,
                "ceiling" => snapshot.ceiling = Some(snapshot.after - 1),
                "id" => snapshot.retries[0].id = 0,
                "duplicate" => snapshot.retries[1].id = snapshot.retries[0].id,
                "wait" => snapshot.retries[0].wait_us = u64::MAX,
                _ => {
                    snapshot.retries = (1..=MAX_RETRY_IDS + 1)
                        .map(|id| Retry {
                            id: i64::try_from(id).unwrap(),
                            attempts: 0,
                            wait_us: 0,
                        })
                        .collect()
                }
            }
            assert!(snapshot.validate(&binding).is_err(), "{mutation}");
        }
        let mut value =
            serde_json::to_value(Snapshot::capture(binding, &debt(now), now, 1_000_000)).unwrap();
        value["profile"] = serde_json::json!({"retired_at": null});
        assert!(serde_json::from_value::<Snapshot>(value).is_err());
    }

    #[test]
    fn malformed_conflict_hints_and_overlap_with_retry_debt_reject_the_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        for mutation in [
            "old_version",
            "zero_id",
            "negative_id",
            "duplicate",
            "retry_overlap",
            "short_digest",
            "long_digest",
            "uppercase_digest",
            "non_hex_digest",
            "count",
        ] {
            let mut snapshot = Snapshot::capture(binding.clone(), &debt(now), now, 1_000_000);
            match mutation {
                "old_version" => snapshot.version = 1,
                "zero_id" => snapshot.conflicts[0].id = 0,
                "negative_id" => snapshot.conflicts[0].id = -1,
                "duplicate" => snapshot.conflicts[1].id = snapshot.conflicts[0].id,
                "retry_overlap" => snapshot.conflicts[0].id = snapshot.retries[0].id,
                "short_digest" => snapshot.conflicts[0].fingerprint = "a".repeat(63),
                "long_digest" => snapshot.conflicts[0].fingerprint = "a".repeat(65),
                "uppercase_digest" => snapshot.conflicts[0].fingerprint = "A".repeat(64),
                "non_hex_digest" => snapshot.conflicts[0].fingerprint = "g".repeat(64),
                _ => {
                    snapshot.conflicts = (1..=MAX_CONFLICT_IDS + 1)
                        .map(|id| ConflictEntry {
                            id: i64::try_from(id + MAX_RETRY_IDS).unwrap(),
                            fingerprint: "a".repeat(64),
                        })
                        .collect();
                }
            }
            assert!(snapshot.validate(&binding).is_err(), "{mutation}");
        }
        let mut value =
            serde_json::to_value(Snapshot::capture(binding, &debt(now), now, 1_000_000)).unwrap();
        value["conflicts"][0]["profile"] = serde_json::json!({"retired_at": null});
        assert!(serde_json::from_value::<Snapshot>(value).is_err());
    }

    #[test]
    fn full_retry_and_conflict_queues_fit_the_existing_checkpoint_byte_bound() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let now = Instant::now();
        let mut cursor = AgentReconcileCursor::default();
        cursor.retries.entries = (0..MAX_RETRY_IDS)
            .map(|offset| Entry {
                id: i64::MAX - i64::try_from(offset).unwrap(),
                attempts: u32::MAX,
                ready_at: now + MAX_BACKOFF,
            })
            .collect();
        for offset in 0..MAX_CONFLICT_IDS {
            assert!(!cursor.conflicts.remember(
                i64::MAX - i64::try_from(MAX_RETRY_IDS + offset).unwrap(),
                "a".repeat(64),
            ));
        }
        let mut store = Store::open(binding.clone()).unwrap();
        store.save(&cursor).unwrap();
        assert!(std::fs::metadata(&store.path).unwrap().len() <= MAX_BYTES);
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.retries.len(), MAX_RETRY_IDS);
        assert_eq!(loaded.conflicts.len(), MAX_CONFLICT_IDS);

        // Include long source paths and the maximum validated generation length
        // as well as maximum-width IDs, attempts and remaining retry delays.
        let long_binding = Binding {
            database: PathBuf::from(format!("/{}", "d".repeat(4095))),
            archive: PathBuf::from(format!("/{}", "a".repeat(4095))),
            generation: "a".repeat(128),
            ..binding
        };
        let snapshot = Snapshot::capture(long_binding.clone(), &cursor, now, i64::MAX);
        snapshot.validate(&long_binding).unwrap();
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        assert!(u64::try_from(bytes.len()).unwrap() <= MAX_BYTES);
    }

    #[test]
    fn binding_rejects_replacement_and_logical_generation_but_not_ordinary_writes() {
        let temp = tempfile::tempdir().unwrap();
        let before = binding(&temp);
        std::fs::write(
            &before.database,
            b"ordinary data write changes length and mtime",
        )
        .unwrap();
        assert_eq!(
            Binding::at(&before.database, &before.archive, "aabb".into()).unwrap(),
            before
        );
        let logical = Binding::at(&before.database, &before.archive, "ccdd".into()).unwrap();
        let snapshot = Snapshot::capture(before.clone(), &debt(Instant::now()), Instant::now(), 1);
        assert!(snapshot.validate(&logical).is_err());
        let other = temp.path().join("other-archive");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(
            before.path().unwrap(),
            Binding::at(&before.database, &other, "aabb".into())
                .unwrap()
                .path()
                .unwrap()
        );
        #[cfg(windows)]
        let retained =
            mcp_agent_mail_core::disk::open_regular_file_no_follow(&before.database).unwrap();
        std::fs::rename(&before.database, temp.path().join("retained-database")).unwrap();
        std::fs::write(&before.database, b"source identity").unwrap();
        #[cfg(windows)]
        match_creation_time(&before.database, &retained);
        let replacement = Binding::at(&before.database, &before.archive, "aabb".into()).unwrap();
        assert_ne!(replacement, before);
        assert!(snapshot.validate(&replacement).is_err());
    }

    #[test]
    fn store_round_trip_holds_exclusive_ownership_until_drop() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        #[cfg(windows)]
        assert!(matches!(
            binding.database.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        let mut store = Store::open(binding.clone()).unwrap();
        assert!(store.load().unwrap().is_none());
        let mut cursor = debt(Instant::now());
        store.save(&cursor).unwrap();
        assert_eq!(store.load().unwrap().unwrap().after, 432);
        assert_eq!(
            Store::open(binding.clone()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!binding.archive.join(".git").exists());
        cursor.after = 433;
        store.save(&cursor).unwrap();
        drop(store);
        let restored = Store::open(binding).unwrap().load().unwrap().unwrap();
        assert_eq!(restored.after, 433);
        assert_eq!(restored.retries.len(), 2);
        assert_eq!(restored.conflicts.len(), 2);
        let mut resumed = AgentReconcileCursor::default();
        restored.restore(
            &mut resumed,
            Instant::now(),
            mcp_agent_mail_db::now_micros(),
        );
        assert!(resumed.conflicts.matches(403, &"a".repeat(64)));
        assert!(resumed.conflicts.matches(404, &"b".repeat(64)));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn unchanged_schedule_keeps_checkpoint_bytes_timestamp_and_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let mut store = Store::open(binding.clone()).unwrap();
        let cursor = conflict_schedule();
        store.save(&cursor).unwrap();
        let saved_at_us = store.load().unwrap().unwrap().saved_at_us;
        let retained = File::open(&store.path).unwrap();
        let before = std::fs::read(&store.path).unwrap();
        let modified = retained.metadata().unwrap().modified().unwrap();
        for _ in 0..3 {
            store.save(&cursor).unwrap();
            assert_eq!(std::fs::read(&store.path).unwrap(), before);
            let named = File::open(&store.path).unwrap();
            assert_eq!(named.metadata().unwrap().modified().unwrap(), modified);
            // Keeping the old handle open prevents inode reuse from making a
            // replaced checkpoint look like the same file on Unix.
            #[cfg(any(unix, windows))]
            assert_eq!(file_identity(&retained), file_identity(&named));
        }
        assert_eq!(store.load().unwrap().unwrap().saved_at_us, saved_at_us);
        assert_eq!(
            Store::open(binding).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn fresh_store_or_identical_replacement_requires_a_new_durable_publication() {
        for reopen in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let binding = binding(&temp);
            let mut store = Store::open(binding.clone()).unwrap();
            let cursor = conflict_schedule();
            store.save(&cursor).unwrap();
            let bytes = std::fs::read(&store.path).unwrap();
            if reopen {
                drop(store);
                store = Store::open(binding).unwrap();
                assert!(store.confirmed.is_none());
            } else {
                std::fs::rename(&store.path, temp.path().join("retained-checkpoint")).unwrap();
                std::fs::write(&store.path, &bytes).unwrap();
                assert!(store.confirmed.is_some());
            }
            let unconfirmed = File::open(&store.path).unwrap();
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
            store.save(&cursor).unwrap();
            let published = File::open(&store.path).unwrap();
            assert_ne!(file_identity(&unconfirmed), file_identity(&published));
            assert!(store.confirmed.is_some());
            let saved = std::fs::read(&store.path).unwrap();
            store.save(&cursor).unwrap();
            assert_eq!(std::fs::read(&store.path).unwrap(), saved);
            assert_eq!(
                file_identity(&published),
                file_identity(&File::open(&store.path).unwrap())
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn in_place_matching_new_state_cannot_inherit_an_older_durable_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let mut store = Store::open(binding.clone()).unwrap();
        let mut cursor = conflict_schedule();
        store.save(&cursor).unwrap();
        let retained = File::open(&store.path).unwrap();
        let modified = retained.metadata().unwrap().modified().unwrap();
        let saved_at_us = store.load().unwrap().unwrap().saved_at_us;
        cursor.conflicts.entries.front_mut().unwrap().fingerprint = "c".repeat(64);
        let external = Snapshot::capture(binding, &cursor, Instant::now(), saved_at_us);
        std::fs::write(&store.path, serde_json::to_vec(&external).unwrap()).unwrap();
        let overwritten = OpenOptions::new().write(true).open(&store.path).unwrap();
        overwritten
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(file_identity(&retained), file_identity(&overwritten));
        let (_, observed) = store.read_snapshot().unwrap().unwrap();
        let confirmed = store.confirmed.as_mut().unwrap();
        assert_ne!(confirmed.digest, observed.digest);
        // Model a filesystem whose metadata stamp cannot distinguish these
        // writes. The real same-inode replacement still has different bytes;
        // its digest must not borrow the old publication's durability proof.
        confirmed.metadata = observed.metadata;
        assert!(confirmed.matches_file(&overwritten).unwrap());
        store.save(&cursor).unwrap();
        assert_ne!(
            file_identity(&retained),
            file_identity(&File::open(&store.path).unwrap())
        );
        assert_eq!(
            store.load().unwrap().unwrap().conflicts[0].fingerprint,
            "c".repeat(64)
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_directory_sync_cannot_certify_matching_named_checkpoint_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(binding(&temp)).unwrap();
        let mut cursor = conflict_schedule();
        store.save(&cursor).unwrap();
        assert!(store.confirmed.is_some());
        cursor.conflicts.entries.front_mut().unwrap().fingerprint = "c".repeat(64);
        FAIL_AFTER_PERSIST.set(true);
        let error = store.save(&cursor).unwrap_err();
        assert_eq!(
            error.to_string(),
            "injected checkpoint directory sync failure"
        );
        assert!(!FAIL_AFTER_PERSIST.get());
        assert!(store.confirmed.is_none());
        assert_eq!(
            store.load().unwrap().unwrap().conflicts[0].fingerprint,
            "c".repeat(64),
            "rename succeeded before the injected directory sync failure"
        );
        let unconfirmed = File::open(&store.path).unwrap();
        store.save(&cursor).unwrap();
        let published = File::open(&store.path).unwrap();
        assert_ne!(file_identity(&unconfirmed), file_identity(&published));
        assert!(store.confirmed.is_some());
        let bytes = std::fs::read(&store.path).unwrap();
        store.save(&cursor).unwrap();
        assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        assert_eq!(
            file_identity(&published),
            file_identity(&File::open(&store.path).unwrap())
        );
    }

    #[test]
    fn changed_watermark_lane_or_ordered_conflicts_replace_the_checkpoint() {
        for change in [
            "after",
            "ceiling",
            "completed",
            "lane",
            "fingerprint",
            "id",
            "order",
            "removed",
            "added",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(binding(&temp)).unwrap();
            let mut cursor = conflict_schedule();
            store.save(&cursor).unwrap();
            let before = std::fs::read(&store.path).unwrap();
            match change {
                "after" => cursor.after = 433,
                "ceiling" => cursor.ceiling = Some(481),
                "completed" => cursor.ceiling = None,
                "lane" => cursor.next_lane_is_retry = false,
                "fingerprint" => {
                    cursor.conflicts.entries.front_mut().unwrap().fingerprint = "c".repeat(64);
                }
                "id" => cursor.conflicts.entries.front_mut().unwrap().id = 405,
                "order" => cursor.conflicts.entries.swap(0, 1),
                "removed" => cursor.conflicts.forget(403),
                "added" => {
                    cursor.conflicts.remember(405, "c".repeat(64));
                }
                _ => unreachable!(),
            }
            store.save(&cursor).unwrap();
            assert_ne!(std::fs::read(&store.path).unwrap(), before, "{change}");
            let saved = store.load().unwrap().unwrap();
            assert_eq!(saved.after, cursor.after, "{change}");
            assert_eq!(saved.ceiling, cursor.ceiling, "{change}");
            assert_eq!(
                saved.next_lane_is_retry, cursor.next_lane_is_retry,
                "{change}"
            );
            assert!(saved.retries.is_empty(), "{change}");
            assert_eq!(
                saved.conflicts,
                cursor.conflicts.entries.iter().cloned().collect::<Vec<_>>(),
                "{change}"
            );
        }
    }

    #[test]
    fn pending_new_or_removed_retry_debt_never_uses_the_unchanged_shortcut() {
        for change in ["pending", "new", "removed"] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(binding(&temp)).unwrap();
            let now = Instant::now();
            let mut cursor = debt(now);
            if change == "new" {
                cursor.retries.entries.clear();
            }
            store.save(&cursor).unwrap();
            #[cfg(any(unix, windows))]
            assert!(store.confirmed.is_some());
            #[cfg(any(unix, windows))]
            let retained = File::open(&store.path).unwrap();
            match change {
                "new" => {
                    cursor.retries.defer(401, now);
                    cursor.retries.defer(401, now);
                    cursor.retries.defer(402, now);
                }
                "removed" => cursor.retries.entries.clear(),
                _ => {}
            }
            let live_debt = cursor
                .retries
                .entries
                .iter()
                .map(|entry| (entry.id, entry.attempts, entry.ready_at))
                .collect::<Vec<_>>();
            store.save(&cursor).unwrap();
            #[cfg(any(unix, windows))]
            assert_ne!(
                file_identity(&retained),
                file_identity(&File::open(&store.path).unwrap()),
                "{change}"
            );
            let saved = store.load().unwrap().unwrap();
            assert_eq!(
                saved
                    .retries
                    .iter()
                    .map(|retry| (retry.id, retry.attempts))
                    .collect::<Vec<_>>(),
                if change == "removed" {
                    vec![]
                } else {
                    vec![(401, 1), (402, 0)]
                },
                "{change}"
            );
            assert_eq!(
                cursor
                    .retries
                    .entries
                    .iter()
                    .map(|entry| (entry.id, entry.attempts, entry.ready_at))
                    .collect::<Vec<_>>(),
                live_debt,
                "saving must not rebase live retry deadlines: {change}"
            );
        }
    }

    #[test]
    fn unchanged_memory_replaces_missing_stale_corrupt_or_obsolete_disk_hints() {
        for change in [
            "missing",
            "stale",
            "conflict",
            "corrupt",
            "oversized",
            "generation",
            "version",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(binding(&temp)).unwrap();
            let cursor = conflict_schedule();
            store.save(&cursor).unwrap();
            match change {
                "missing" => {
                    std::fs::rename(&store.path, temp.path().join("retained-checkpoint")).unwrap();
                }
                "corrupt" => std::fs::write(&store.path, b"{corrupt").unwrap(),
                "oversized" => {
                    std::fs::write(
                        &store.path,
                        vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1],
                    )
                    .unwrap();
                }
                _ => {
                    let mut saved = store.load().unwrap().unwrap();
                    saved.saved_at_us = 1;
                    match change {
                        "stale" => saved.after = 431,
                        "conflict" => saved.conflicts[0].fingerprint = "c".repeat(64),
                        "generation" => saved.binding.generation = "ccdd".into(),
                        "version" => saved.version += 1,
                        _ => unreachable!(),
                    }
                    std::fs::write(&store.path, serde_json::to_vec(&saved).unwrap()).unwrap();
                }
            }
            // No in-memory last-save marker can certify what is now at the
            // named path, even though this cursor has not changed at all.
            store.save(&cursor).unwrap();
            let saved = store.load().unwrap().unwrap();
            assert_ne!(saved.saved_at_us, 1, "{change}");
            assert_eq!(saved.after, 432, "{change}");
            assert_eq!(saved.ceiling, Some(480), "{change}");
            assert!(saved.next_lane_is_retry, "{change}");
            assert!(saved.retries.is_empty(), "{change}");
            assert_eq!(saved.conflicts[0].fingerprint, "a".repeat(64), "{change}");
            assert_eq!(saved.conflicts[1].fingerprint, "b".repeat(64), "{change}");
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn equal_checkpoint_does_not_bypass_source_or_lock_identity_changes() {
        for changed_source in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let binding = binding(&temp);
            let mut store = Store::open(binding.clone()).unwrap();
            let cursor = conflict_schedule();
            store.save(&cursor).unwrap();
            let before = std::fs::read(&store.path).unwrap();
            let path = if changed_source {
                binding.database.clone()
            } else {
                store.lock_path.clone()
            };
            #[cfg(windows)]
            let original = File::open(&path).unwrap();
            std::fs::rename(&path, temp.path().join("retained-authority")).unwrap();
            std::fs::write(&path, b"replacement authority").unwrap();
            #[cfg(windows)]
            match_creation_time(&path, &original);
            let _new_owner = (!changed_source).then(|| Store::open(binding.clone()).unwrap());
            assert_eq!(
                store.save(&cursor).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(store.confirmed.is_none());
            assert_eq!(std::fs::read(&store.path).unwrap(), before);
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement authority");
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn equal_hardlinked_checkpoint_is_rejected_without_touching_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(binding(&temp)).unwrap();
        let cursor = conflict_schedule();
        store.save(&cursor).unwrap();
        let before = std::fs::read(&store.path).unwrap();
        let evidence = temp.path().join("retained-checkpoint-evidence");
        std::fs::hard_link(&store.path, &evidence).unwrap();
        assert_eq!(
            store.save(&cursor).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(store.confirmed.is_none());
        assert_eq!(std::fs::read(&store.path).unwrap(), before);
        assert_eq!(std::fs::read(&evidence).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn equal_symlinked_checkpoint_and_directory_obstruction_are_not_replaced() {
        for linked in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut store = Store::open(binding(&temp)).unwrap();
            let cursor = conflict_schedule();
            store.save(&cursor).unwrap();
            let before = std::fs::read(&store.path).unwrap();
            let evidence = temp.path().join("retained-checkpoint-evidence");
            std::fs::rename(&store.path, &evidence).unwrap();
            if linked {
                std::os::unix::fs::symlink(&evidence, &store.path).unwrap();
            } else {
                std::fs::create_dir(&store.path).unwrap();
            }
            assert_eq!(
                store.save(&cursor).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(store.confirmed.is_none());
            assert_eq!(std::fs::read(&evidence).unwrap(), before);
            assert_eq!(
                std::fs::symlink_metadata(&store.path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                linked
            );
        }
    }

    #[test]
    fn oversized_and_corrupt_checkpoint_reads_leave_bytes_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(binding(&temp)).unwrap();
        for bytes in [
            b"{corrupt".to_vec(),
            vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1],
        ] {
            std::fs::write(&store.path, &bytes).unwrap();
            assert!(store.load().is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_hardlinked_checkpoint_and_lock_paths_fail_closed() {
        for is_lock in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let binding = binding(&temp);
            let checkpoint = binding.path().unwrap();
            let lock = checkpoint.with_extension("json.lock");
            let unsafe_path = if is_lock { &lock } else { &checkpoint };
            let evidence = temp.path().join(if is_lock {
                "lock-evidence"
            } else {
                "checkpoint-evidence"
            });
            std::fs::write(&evidence, b"preserve me").unwrap();
            std::fs::hard_link(&evidence, unsafe_path).unwrap();

            let error = Store::open(binding).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
            assert!(error.to_string().contains("multiple hard links"), "{error}");
            assert_eq!(std::fs::read(&evidence).unwrap(), b"preserve me");
            assert_eq!(std::fs::read(unsafe_path).unwrap(), b"preserve me");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_replaced_lock_with_equal_creation_time_cannot_publish() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(&temp);
        let mut original = Store::open(binding.clone()).unwrap();
        let mut cursor = debt(Instant::now());
        original.save(&cursor).unwrap();
        let previous = std::fs::read(&original.path).unwrap();
        std::fs::rename(&original.lock_path, temp.path().join("retained-lock")).unwrap();
        std::fs::write(&original.lock_path, b"replacement owner").unwrap();
        match_creation_time(&original.lock_path, &original.lock);
        let mut replacement = Store::open(binding).unwrap();
        assert!(original.load().is_err());
        cursor.after = 433;
        assert!(original.save(&cursor).is_err());
        assert_eq!(std::fs::read(&original.path).unwrap(), previous);
        replacement.save(&cursor).unwrap();
        assert_eq!(replacement.load().unwrap().unwrap().after, 433);
        let published = std::fs::read(&replacement.path).unwrap();
        assert!(original.save(&cursor).is_err());
        assert_eq!(std::fs::read(&replacement.path).unwrap(), published);
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
        let mut store = Store::open(binding.clone()).unwrap();
        std::fs::rename(&store.lock_path, temp.path().join("retained-lock")).unwrap();
        std::fs::write(&store.lock_path, b"new lock identity").unwrap();
        assert!(store.save(&debt(Instant::now())).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"retained foreign bytes");
        assert_eq!(
            std::fs::read(&store.lock_path).unwrap(),
            b"new lock identity"
        );
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
            .spawn()
            .unwrap();
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
        assert!(
            !expired && status.success() && output.contains("1 passed; 0 failed"),
            "{name}: timeout={expired}, {output}"
        );
        true
    }

    fn with_mailbox(test: impl FnOnce(&Cx, &DbPool, &Config, &Path)) {
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
                warmup_connections: 0,
                ..Default::default()
            })
            .unwrap();
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

    fn archive_deregistration_conflict(config: &Config) -> serde_json::Value {
        let relative = "projects/project/agents/BlueLake/profile.json";
        let mut conflicting = profile(config);
        conflicting["deregistered_at"] =
            serde_json::json!(mcp_agent_mail_db::micros_to_iso(234_567_891));
        conflicting["extension"] = serde_json::json!({"preserve": "archived evidence"});
        crate::write_json(&config.storage_root.join(relative), &conflicting, true).unwrap();
        crate::commit_paths_with_retry(
            &config.storage_root,
            config,
            "fixture: archived deregistration without live ledger",
            &[relative],
        )
        .unwrap();
        assert_eq!(profile(config), conflicting);
        conflicting
    }

    #[test]
    fn restarted_conflict_suppression_keeps_evidence_and_repairs_changed_live_ledger() {
        with_mailbox(|cx, pool, config, _| {
            let stop = AtomicBool::new(false);
            let mut first = AgentReconcileCursor::default();
            assert_eq!(
                first
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .repaired,
                1
            );
            let conflicting = archive_deregistration_conflict(config);
            let deferred = first
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(
                (deferred.deferred, deferred.conflict_len, deferred.retry_len),
                (1, 1, 0)
            );
            assert_eq!(deferred.suppressed_conflicts, 0);
            assert_eq!(profile(config), conflicting);
            let binding = first.retries.persistence.binding.clone().unwrap();
            assert_eq!(
                first
                    .retries
                    .persistence
                    .store
                    .as_ref()
                    .unwrap()
                    .load()
                    .unwrap()
                    .unwrap()
                    .conflicts
                    .len(),
                1
            );
            drop(first);

            // A new daemon owns neither the old cursor nor its process-local
            // pool identity. Only the validated durable binding survives.
            let reopened = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                warmup_connections: 0,
                ..Default::default()
            })
            .unwrap();
            assert_ne!(pool.sqlite_identity_key(), reopened.sqlite_identity_key());
            assert_eq!(
                Binding::capture(cx, &reopened, config, &stop).unwrap(),
                binding
            );
            let mut restarted = AgentReconcileCursor::default();
            let repeated = restarted
                .reconcile_checkpointed(cx, &reopened, config, &stop)
                .unwrap();
            assert_eq!(
                (
                    repeated.suppressed_conflicts,
                    repeated.deferred,
                    repeated.repaired,
                    repeated.conflict_len,
                ),
                (1, 0, 0, 1)
            );
            assert_eq!(profile(config), conflicting);

            outcome(block_on(mcp_agent_mail_db::queries::deregister_agent(
                cx,
                &reopened,
                101,
                234_567_891,
            )))
            .unwrap();
            let repaired = restarted
                .reconcile_checkpointed(cx, &reopened, config, &stop)
                .unwrap();
            assert_eq!(
                (
                    repaired.repaired,
                    repaired.deferred,
                    repaired.suppressed_conflicts,
                    repaired.conflict_len,
                    repaired.retry_len,
                ),
                (1, 0, 0, 0, 0)
            );
            let archived = profile(config);
            assert_eq!(archived["deregistered_at"], conflicting["deregistered_at"]);
            assert_eq!(archived["extension"], conflicting["extension"]);
            assert_eq!(archived["contact_policy"], "block_all");
            let saved = restarted
                .retries
                .persistence
                .store
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .unwrap();
            assert!(saved.conflicts.is_empty());
            assert!(saved.retries.is_empty());
        });
    }

    #[test]
    fn gh369_competing_checkpoint_owner_defers_agent_repair_without_memory_fallback() {
        with_mailbox(|cx, pool, config, _| {
            let stop = AtomicBool::new(false);
            let mut owner =
                Store::open(Binding::capture(cx, pool, config, &stop).unwrap()).unwrap();
            owner.save(&AgentReconcileCursor::default()).unwrap();
            let saved = std::fs::read(&owner.path).unwrap();
            let profile_path = config
                .storage_root
                .join("projects/project/agents/BlueLake/profile.json");
            let mut contender = AgentReconcileCursor::default();
            for _ in 0..3 {
                let error = contender
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap_err();
                assert_eq!(
                    error,
                    "agent reconciliation deferred: checkpoint is owned by another worker"
                );
                assert_eq!(contender.after, 0);
                assert!(!contender.retries.persistence.restore_attempted);
                assert!(!contender.retries.persistence.warned);
                assert!(!profile_path.exists());
                assert_eq!(std::fs::read(&owner.path).unwrap(), saved);
            }
            drop(owner);
            let resumed = contender
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(resumed.repaired, 1);
            profile(config);
            let store = contender.retries.persistence.store.as_ref().unwrap();
            assert_eq!(store.load().unwrap().unwrap().after, contender.after);
        });
    }

    #[test]
    fn agent_generation_initializes_once_and_restarts_without_reservations() {
        with_mailbox(|cx, pool, config, _| {
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            assert!(
                conn.query_sync("SELECT * FROM db_identity", &[])
                    .unwrap()
                    .is_empty()
            );
            assert!(
                conn.query_sync("SELECT id FROM file_reservations", &[])
                    .unwrap()
                    .is_empty()
            );
            drop(conn);
            let stop = AtomicBool::new(false);
            let mut first = AgentReconcileCursor::default();
            assert_eq!(
                first
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .repaired,
                1
            );
            let bound = Binding::capture(cx, pool, config, &stop).unwrap();
            assert!(!bound.generation.is_empty());
            assert!(bound.generation.bytes().all(|b| b.is_ascii_hexdigit()));
            let bytes = std::fs::read(bound.path().unwrap()).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("never-checkpoint-this-token"));
            let original_profile = profile(config);
            drop(first);
            let mut restarted = AgentReconcileCursor::default();
            assert_eq!(
                restarted
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .unchanged,
                1
            );
            assert_eq!(Binding::capture(cx, pool, config, &stop).unwrap(), bound);
            assert_eq!(profile(config), original_profile);
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT singleton,generation_id FROM db_identity", &[])
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].get_named::<String>("generation_id").unwrap(),
                bound.generation
            );
            assert!(
                conn.query_sync("SELECT id FROM file_reservations", &[])
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                conn.query_sync("SELECT registration_token FROM agents WHERE id=101", &[])
                    .unwrap()[0]
                    .get_named::<String>("registration_token")
                    .unwrap(),
                "never-checkpoint-this-token"
            );
        });
    }

    #[test]
    fn agent_generation_readonly_stopped_and_disk_pressure_do_not_seed_or_create_sidecars() {
        for mode in 0..3 {
            with_mailbox(|cx, pool, config, scratch| {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert!(
                    conn.query_sync("SELECT * FROM db_identity", &[])
                        .unwrap()
                        .is_empty()
                );
                if mode == 0 {
                    conn.execute_raw("PRAGMA query_only=ON").unwrap();
                }
                drop(conn);
                let pressure = &mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level;
                let saved = pressure.load();
                if mode == 2 {
                    pressure.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
                }
                let stop = AtomicBool::new(mode == 1);
                let mut cursor = AgentReconcileCursor::default();
                let result = cursor.reconcile_checkpointed(cx, pool, config, &stop);
                pressure.set(saved);
                if mode == 1 {
                    assert!(result.unwrap().interrupted);
                } else {
                    let error = result.unwrap_err();
                    assert!(
                        error.contains(if mode == 0 {
                            "query-only"
                        } else {
                            "critical disk pressure"
                        }),
                        "{error}"
                    );
                }
                assert!(!config.storage_root.join(".git").exists());
                assert!(std::fs::read_dir(scratch).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".am-agent-reconcile-")
                }));
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert!(
                    conn.query_sync("SELECT * FROM db_identity", &[])
                        .unwrap()
                        .is_empty()
                );
                if mode == 0 {
                    conn.execute_raw("PRAGMA query_only=OFF").unwrap();
                }
            });
        }
    }

    #[test]
    fn agent_generation_never_replaces_malformed_or_nonempty_identity() {
        for insert in [
            "(0,'')",
            "(0,'not-a-generation')",
            "(0,X'0102')",
            "(1,'aabb')",
            "(0,'aabb'),(1,'ccdd')",
        ] {
            with_mailbox(|cx, pool, config, scratch| {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                conn.execute_raw("ALTER TABLE db_identity RENAME TO retained_identity; CREATE TABLE db_identity(singleton INTEGER PRIMARY KEY,generation_id)").unwrap();
                conn.execute_raw(&format!("INSERT INTO db_identity VALUES {insert}"))
                    .unwrap();
                let values = |connection: &mcp_agent_mail_db::DbConn| {
                    connection
                        .query_sync("SELECT * FROM db_identity ORDER BY singleton", &[])
                        .unwrap()
                        .into_iter()
                        .map(|row| {
                            (
                                row.get_named::<i64>("singleton").unwrap(),
                                row.get_by_name("generation_id").unwrap().clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                let before = values(&conn);
                drop(conn);
                let result = AgentReconcileCursor::default().reconcile_checkpointed(
                    cx,
                    pool,
                    config,
                    &AtomicBool::new(false),
                );
                assert!(
                    result.is_err(),
                    "malformed authority {insert} must not become a new generation"
                );
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                assert_eq!(values(&conn), before);
                assert!(!config.storage_root.join(".git").exists());
                assert!(std::fs::read_dir(scratch).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".am-agent-reconcile-")
                }));
            });
        }
    }

    #[test]
    fn agent_generation_reads_actual_initializer_winner_and_refuses_ignored_writes() {
        for competing in [false, true] {
            with_mailbox(|cx, pool, config, _| {
                let conn = outcome(block_on(pool.acquire(cx))).unwrap();
                let other = if competing {
                    "INSERT OR IGNORE INTO db_identity VALUES(0,'cafefeed');"
                } else {
                    ""
                };
                conn.execute_raw(&format!("CREATE TRIGGER intercept_generation BEFORE INSERT ON db_identity BEGIN {other} SELECT RAISE(IGNORE); END")).unwrap();
                drop(conn);
                let result = Binding::capture(cx, pool, config, &AtomicBool::new(false));
                if competing {
                    assert_eq!(result.unwrap().generation, "cafefeed");
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .contains("requires one database generation")
                    );
                }
                assert!(!config.storage_root.join(".git").exists());
            });
        }
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
                repaired += reconcile_agent_batch(cx, pool, config, &mut seeded, &stop)
                    .unwrap()
                    .repaired;
            }
            assert_eq!(repaired, 41);
            let mut first = AgentReconcileCursor::default();
            assert_eq!(
                first
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .unchanged,
                32
            );
            assert_eq!(first.after, 32);
            outcome(block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
                cx,
                pool,
                101,
                Some(123_456_789),
            )))
            .unwrap();
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
            assert!(
                fence.join().unwrap().is_ok(),
                "repair waited on the publication fence"
            );
            let report = report.unwrap();
            assert_eq!(
                (
                    report.scanned,
                    report.repaired,
                    report.deferred_transient,
                    report.retry_len
                ),
                (9, 0, 1, 1)
            );
            let path = first
                .retries
                .persistence
                .store
                .as_ref()
                .unwrap()
                .path
                .clone();
            let durable: Snapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                durable
                    .retries
                    .iter()
                    .map(|retry| retry.id)
                    .collect::<Vec<_>>(),
                [101]
            );
            assert_eq!(durable.after, 101);
            drop(first); // releases the actual OS lock, not just the retry payload

            // Change source lifecycle without delivering its archive write.
            outcome(block_on(mcp_agent_mail_db::queries::deregister_agent(
                cx,
                pool,
                101,
                234_567_891,
            )))
            .unwrap();
            let mut restarted = AgentReconcileCursor::default();
            let result = restarted
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(
                (result.repaired, result.deferred, result.retry_len),
                (1, 0, 0)
            );
            assert_eq!(
                profile(config)["deregistered_at"],
                mcp_agent_mail_db::micros_to_iso(234_567_891)
            );
            assert_eq!(profile(config)["contact_policy"], "block_all");
            assert_eq!(
                restarted
                    .retries
                    .persistence
                    .store
                    .as_ref()
                    .unwrap()
                    .load()
                    .unwrap()
                    .unwrap()
                    .retries
                    .len(),
                0
            );
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            let rows = conn
                .query_sync("SELECT registration_token FROM agents WHERE id=101", &[])
                .unwrap();
            assert_eq!(
                rows[0].get_named::<String>("registration_token").unwrap(),
                "never-checkpoint-this-token"
            );
        });
    }

    #[test]
    fn competing_cursor_defers_without_consuming_work_and_cancelled_cursor_creates_no_store() {
        with_mailbox(|cx, pool, config, _| {
            let mut cancelled = AgentReconcileCursor::default();
            let stop = AtomicBool::new(true);
            assert!(
                cancelled
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .interrupted
            );
            assert!(cancelled.retries.persistence.store.is_none());
            stop.store(false, Ordering::Release);
            let mut owner = AgentReconcileCursor::default();
            assert_eq!(
                owner
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .repaired,
                1
            );
            let bytes = std::fs::read(
                owner
                    .retries
                    .persistence
                    .store
                    .as_ref()
                    .unwrap()
                    .path
                    .clone(),
            )
            .unwrap();
            let mut contender = AgentReconcileCursor::default();
            let error = contender
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap_err();
            assert_eq!(
                error,
                "agent reconciliation deferred: checkpoint is owned by another worker"
            );
            assert_eq!(contender.after, 0);
            assert!(contender.retries.entries.is_empty());
            assert_eq!(
                std::fs::read(
                    owner
                        .retries
                        .persistence
                        .store
                        .as_ref()
                        .unwrap()
                        .path
                        .clone()
                )
                .unwrap(),
                bytes
            );
            drop(owner);
            assert_eq!(
                contender
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .unchanged,
                1
            );
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
        });
    }

    #[test]
    fn query_only_and_foreign_sources_create_no_checkpoint_or_archive() {
        with_mailbox(|cx, pool, config, scratch| {
            let path = Binding::capture(cx, pool, config, &AtomicBool::new(false))
                .unwrap()
                .path()
                .unwrap();
            let readonly = DbPool::new_query_only(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                ..Default::default()
            })
            .unwrap();
            let mut cursor = AgentReconcileCursor::default();
            let stop = AtomicBool::new(false);
            let error = cursor
                .reconcile_checkpointed(cx, &readonly, config, &stop)
                .unwrap_err();
            assert!(error.contains("query-only snapshots"), "{error}");
            assert!(!path.exists() && !path.with_extension("json.lock").exists());
            assert!(!config.storage_root.join("projects").exists());

            let foreign = scratch.join("foreign.sqlite3");
            std::fs::write(&foreign, b"unrelated source").unwrap();
            let other = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&foreign),
                ..config.clone()
            };
            let error = cursor
                .reconcile_checkpointed(cx, pool, &other, &stop)
                .unwrap_err();
            assert!(
                error.contains("not the configured live database"),
                "{error}"
            );
            assert_eq!(std::fs::read(foreign).unwrap(), b"unrelated source");
            assert!(!path.exists() && !path.with_extension("json.lock").exists());
            assert!(!config.storage_root.join("projects").exists());
        });
    }

    fn exercise_checkpoint_io_failure(reopen_pool: bool) {
        with_mailbox(|cx, pool, config, scratch| {
            let stop = AtomicBool::new(false);
            let mut cursor = AgentReconcileCursor::default();
            assert_eq!(
                cursor
                    .reconcile_checkpointed(cx, pool, config, &stop)
                    .unwrap()
                    .repaired,
                1
            );
            let path = cursor
                .retries
                .persistence
                .store
                .as_ref()
                .unwrap()
                .path
                .clone();
            std::fs::rename(&path, scratch.join("old-checkpoint")).unwrap();
            std::fs::create_dir(&path).unwrap();
            // A retained debt below a completed watermark must survive both a
            // failed save and reappearance of the OLD valid checkpoint.
            cursor.retries.defer(999_999, Instant::now());
            cursor.retries.defer(999_999, Instant::now() + MAX_BACKOFF);
            cursor.conflicts.remember(999_998, "a".repeat(64));
            let report = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!(report.retry_len, 1);
            assert!(cursor.conflicts.matches(999_998, &"a".repeat(64)));
            assert!(cursor.retries.persistence.warned);
            std::fs::rename(&path, scratch.join("retained-checkpoint-obstruction")).unwrap();
            std::fs::rename(scratch.join("old-checkpoint"), &path).unwrap();
            let binding = cursor.retries.persistence.binding.clone().unwrap();
            let reopened = reopen_pool.then(|| {
                DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                    database_url: config.database_url.clone(),
                    storage_root: Some(config.storage_root.clone()),
                    min_connections: 1,
                    max_connections: 1,
                    warmup_connections: 0,
                    ..Default::default()
                })
                .unwrap()
            });
            let source = reopened.as_ref().unwrap_or(pool);
            assert_eq!(
                source.sqlite_identity_key() != pool.sqlite_identity_key(),
                reopen_pool,
                "a cached pool would not exercise source-key turnover"
            );
            assert_eq!(
                Binding::capture(cx, source, config, &stop).unwrap(),
                binding
            );
            // Pool initialization can be slow under contention. Establish the
            // observed cooldown after it completes instead of racing a timer.
            cursor.retries.entries.front_mut().unwrap().ready_at = Instant::now() + MAX_BACKOFF;
            let debt = cursor
                .retries
                .entries
                .iter()
                .map(|entry| (entry.id, entry.attempts, entry.ready_at))
                .collect::<Vec<_>>();
            // Linux flock belongs to the shared open-file description. Keeping
            // a duplicate makes an accidental drop/reopen fail deterministically
            // instead of allowing a new owner through a temporary lock gap.
            #[cfg(target_os = "linux")]
            let _retained_lock = cursor
                .retries
                .persistence
                .store
                .as_ref()
                .unwrap()
                .lock
                .try_clone()
                .unwrap();
            let report = cursor
                .reconcile_checkpointed(cx, source, config, &stop)
                .unwrap();
            assert_eq!(report.retry_len, 1);
            assert_eq!(cursor.source_identity, source.sqlite_identity_key());
            assert_eq!(
                cursor
                    .retries
                    .entries
                    .iter()
                    .map(|entry| (entry.id, entry.attempts, entry.ready_at))
                    .collect::<Vec<_>>(),
                debt,
                "pool turnover must not reload or rebase the live cooldown"
            );
            assert!(
                cursor.conflicts.matches(999_998, &"a".repeat(64)),
                "pool turnover must not reload older conflict hints"
            );
            assert!(!cursor.retries.persistence.warned);
            assert_eq!(
                Store::open(binding).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            let saved = cursor
                .retries
                .persistence
                .store
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .unwrap();
            assert_eq!(saved.retries[0].id, 999_999);
            assert_eq!(saved.conflicts[0].id, 999_998);
        });
    }

    #[test]
    fn checkpoint_io_failure_never_reloads_older_progress_over_live_retry_debt() {
        exercise_checkpoint_io_failure(false);
    }

    #[test]
    fn same_source_pool_recreation_retains_unsaved_agent_debt_and_checkpoint_owner() {
        exercise_checkpoint_io_failure(true);
    }

    #[test]
    fn pool_recreation_without_a_retained_agent_binding_starts_a_new_scan() {
        with_mailbox(|cx, pool, config, _| {
            let stop = AtomicBool::new(false);
            let mut cursor = AgentReconcileCursor::default();
            reconcile_agent_batch(cx, pool, config, &mut cursor, &stop).unwrap();
            assert!(cursor.retries.persistence.binding.is_none());
            cursor.retries.defer(999_999, Instant::now() + MAX_BACKOFF);
            cursor.conflicts.remember(999_998, "a".repeat(64));
            let reopened = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                warmup_connections: 0,
                ..Default::default()
            })
            .unwrap();
            assert_ne!(pool.sqlite_identity_key(), reopened.sqlite_identity_key());
            let report = cursor
                .reconcile_checkpointed(cx, &reopened, config, &stop)
                .unwrap();
            assert_eq!((report.unchanged, report.retry_len), (1, 0));
            assert_eq!(cursor.conflicts.len(), 0);
            assert_eq!(cursor.source_identity, reopened.sqlite_identity_key());
        });
    }

    #[test]
    fn pool_recreation_with_a_changed_agent_generation_rechecks_conflicts_and_rejects_old_debt() {
        with_mailbox(|cx, pool, config, _| {
            let stop = AtomicBool::new(false);
            let mut cursor = AgentReconcileCursor::default();
            cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            let conflicting = archive_deregistration_conflict(config);
            let report = cursor
                .reconcile_checkpointed(cx, pool, config, &stop)
                .unwrap();
            assert_eq!((report.deferred, report.conflict_len), (1, 1));
            let binding = cursor.retries.persistence.binding.clone().unwrap();
            cursor.retries.defer(999_999, Instant::now() + MAX_BACKOFF);
            let mut store = cursor.retries.persistence.store.take().unwrap();
            store.save(&cursor).unwrap();
            cursor.retries.persistence.store = Some(store);
            let generation = if binding.generation == "ccdd" {
                "eeff"
            } else {
                "ccdd"
            };
            let conn = outcome(block_on(pool.acquire(cx))).unwrap();
            conn.execute_sync(
                "UPDATE db_identity SET generation_id = ? WHERE singleton = 0",
                &[generation.into()],
            )
            .unwrap();
            drop(conn);
            let reopened = DbPool::new(&mcp_agent_mail_db::DbPoolConfig {
                database_url: config.database_url.clone(),
                storage_root: Some(config.storage_root.clone()),
                min_connections: 1,
                max_connections: 1,
                warmup_connections: 0,
                ..Default::default()
            })
            .unwrap();
            assert_ne!(pool.sqlite_identity_key(), reopened.sqlite_identity_key());
            assert_ne!(
                Binding::capture(cx, &reopened, config, &stop).unwrap(),
                binding
            );
            let report = cursor
                .reconcile_checkpointed(cx, &reopened, config, &stop)
                .unwrap();
            assert_eq!(
                (
                    report.deferred,
                    report.suppressed_conflicts,
                    report.conflict_len,
                    report.retry_len,
                ),
                (1, 0, 1, 0),
                "the new database generation must reclassify the conflict"
            );
            assert_eq!(profile(config), conflicting);
            let store = cursor.retries.persistence.store.as_ref().unwrap();
            assert_eq!(
                store.load().unwrap().unwrap().binding.generation,
                generation
            );
        });
    }
}
